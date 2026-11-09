//! Unit tests for mod.rs: config helpers, the connection loop over in-memory
//! streams, and real servers on 127.0.0.1:0.

use super::*;
use std::io::{Cursor, Read};
use std::net::SocketAddrV6;
use std::sync::mpsc;

use crate::digest;

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path()).unwrap();
    (dir, s)
}

/// Run the connection loop over `input`; returns everything written.
fn converse(store: &Store, input: &[u8], stop: bool, verbose: bool) -> String {
    let mut out = Vec::new();
    let flag = AtomicBool::new(stop);
    serve_connection(
        store,
        verbose,
        &flag,
        &mut Cursor::new(input.to_vec()),
        &mut out,
    );
    String::from_utf8_lossy(&out).into_owned()
}

fn statuses(out: &str) -> Vec<&str> {
    out.match_indices("HTTP/1.1 ")
        .map(|(i, _)| &out[i + 9..i + 12])
        .collect()
}

struct FailWriter;
impl Write for FailWriter {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("closed"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn config_helpers() {
    let p = storage_root(Some(Path::new("/x")), None).unwrap();
    assert_eq!(p, PathBuf::from("/x"));
    let p = storage_root(None, Some("/home/u".into())).unwrap();
    assert_eq!(p, PathBuf::from("/home/u/.local/share/crane/registry"));
    assert!(storage_root(None, None).is_err());

    assert_eq!(listen_address(Some("0.0.0.0:1"), Some("9")), "0.0.0.0:1");
    assert_eq!(listen_address(None, Some("9")), "127.0.0.1:9");
    assert_eq!(listen_address(None, Some("nope")), "127.0.0.1:5050");
    assert_eq!(listen_address(None, Some("70000")), "127.0.0.1:5050");
    assert_eq!(listen_address(None, None), "127.0.0.1:5050");

    let v4: SocketAddr = "0.0.0.0:7".parse().unwrap();
    assert_eq!(wake_addr(v4), "127.0.0.1:7".parse().unwrap());
    let v6 = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 7, 0, 0));
    assert_eq!(wake_addr(v6), "[::1]:7".parse().unwrap());
    let fixed: SocketAddr = "10.1.2.3:7".parse().unwrap();
    assert_eq!(wake_addr(fixed), fixed);
}

#[test]
fn lock_is_exclusive() {
    let dir = tempfile::tempdir().unwrap();
    let first = lock_storage(dir.path()).unwrap();
    let err = lock_storage(dir.path()).unwrap_err().to_string();
    assert!(err.contains("in use by another registry server"), "{err}");
    drop(first);
    lock_storage(dir.path()).unwrap();
    // Root is a regular file: cannot even open the lock file.
    let f = dir.path().join("file");
    fs::write(&f, b"").unwrap();
    let err = lock_storage(&f).unwrap_err().to_string();
    assert!(err.contains("cannot open"), "{err}");
}

#[test]
fn keep_alive_pipelining_and_head() {
    let (_d, s) = store();
    let input =
        b"GET /v2/ HTTP/1.1\r\n\r\nHEAD /v2/ HTTP/1.1\r\n\r\nGET /v2/_catalog HTTP/1.1\r\n\r\n";
    let out = converse(&s, input, false, true);
    assert_eq!(statuses(&out), ["200", "200", "200"]);
    // HEAD has Content-Length but no body: the next response follows directly.
    assert!(
        out.contains("Content-Length: 2\r\n\r\n{}HTTP/1.1 200 OK"),
        "{out}"
    );
    assert!(
        out.contains("Content-Length: 2\r\n\r\nHTTP/1.1 200 OK"),
        "{out}"
    );
    assert!(out.ends_with("{\"repositories\":[]}"), "{out}");
    assert!(!out.contains("Connection: close"));
}

#[test]
fn connection_close_and_stop_flag() {
    let (_d, s) = store();
    let two = b"GET /v2/ HTTP/1.1\r\nConnection: close\r\n\r\nGET /v2/ HTTP/1.1\r\n\r\n";
    let out = converse(&s, two, false, false);
    assert_eq!(statuses(&out), ["200"]);
    assert!(out.contains("Connection: close"));
    let out = converse(
        &s,
        b"GET /v2/ HTTP/1.0\r\n\r\nGET /v2/ HTTP/1.1\r\n\r\n",
        false,
        false,
    );
    assert_eq!(statuses(&out), ["200"]);
    // A stopping server answers the request in hand, then closes.
    let out = converse(
        &s,
        b"GET /v2/ HTTP/1.1\r\n\r\nGET /v2/ HTTP/1.1\r\n\r\n",
        true,
        false,
    );
    assert_eq!(statuses(&out), ["200"]);
    assert!(out.contains("Connection: close"));
}

#[test]
fn garbage_gets_400_and_close() {
    let (_d, s) = store();
    let out = converse(&s, &[0x16, 0x03, 0x01, 0x00, 0xa5, 0x01], false, false);
    assert_eq!(statuses(&out), ["400"]);
    assert!(out.contains("Connection: close"));
    assert!(out.contains("\"code\":\"UNSUPPORTED\""));
    // A cut-off head (not malformed, just closed) gets no answer.
    assert_eq!(converse(&s, b"GET /v2/ HTTP/1.1\r\nHost", false, false), "");
    assert_eq!(converse(&s, b"", false, false), "");
}

#[test]
fn expect_continue() {
    let (_d, s) = store();
    let d = digest::of(b"hello");
    let req = format!(
        "POST /v2/r/blobs/uploads/?digest={d} HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 5\r\n\r\nhello"
    );
    let out = converse(&s, req.as_bytes(), false, false);
    assert_eq!(statuses(&out), ["100", "201"]);
    // Writer gone before the 100: nothing else happens.
    let (_d2, s) = store();
    let flag = AtomicBool::new(false);
    serve_connection(
        &s,
        false,
        &flag,
        &mut Cursor::new(req.into_bytes()),
        &mut FailWriter,
    );
    assert!(!s.has_blob(&d));
}

#[test]
fn chunked_upload_over_the_wire() {
    let (_d, s) = store();
    let d = digest::of(b"hello world");
    let req = format!(
        "POST /v2/r/blobs/uploads/?digest={d} HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\
         5\r\nhello\r\n6\r\n world\r\n0\r\n\r\nGET /v2/r/blobs/{d} HTTP/1.1\r\n\r\n"
    );
    let out = converse(&s, req.as_bytes(), false, false);
    assert_eq!(statuses(&out), ["201", "200"]);
    assert!(out.ends_with("\r\n\r\nhello world"), "{out}");
}

#[test]
fn bad_bodies() {
    let (_d, s) = store();
    // Malformed chunked body: 400 and close, whatever the handler said.
    let d = digest::of(b"x");
    let req = format!(
        "POST /v2/r/blobs/uploads/?digest={d} HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nGET /v2/ HTTP/1.1\r\n\r\n"
    );
    let out = converse(&s, req.as_bytes(), false, false);
    assert_eq!(statuses(&out), ["400"]);
    assert!(out.contains("BLOB_UPLOAD_INVALID") && out.contains("Connection: close"));
    // Unread body within the drain limit: the connection stays usable.
    let req = "GET /v2/ HTTP/1.1\r\nContent-Length: 3\r\n\r\nabcGET /v2/ HTTP/1.1\r\n\r\n";
    assert_eq!(
        statuses(&converse(&s, req.as_bytes(), false, false)),
        ["200", "200"]
    );
    // Unread body over the drain limit: answer, then close.
    let big = DRAIN_LIMIT + 10;
    let mut req = format!("GET /v2/ HTTP/1.1\r\nContent-Length: {big}\r\n\r\n").into_bytes();
    req.resize(req.len() + big as usize, b'a');
    req.extend_from_slice(b"GET /v2/ HTTP/1.1\r\n\r\n");
    let out = converse(&s, &req, false, false);
    assert_eq!(statuses(&out), ["200"]);
    assert!(out.contains("Connection: close"));
}

#[test]
fn write_failure_ends_connection() {
    let (_d, s) = store();
    let flag = AtomicBool::new(false);
    let input = b"GET /v2/ HTTP/1.1\r\n\r\nGET /v2/ HTTP/1.1\r\n\r\n".to_vec();
    serve_connection(&s, false, &flag, &mut Cursor::new(input), &mut FailWriter);
    // Also for garbage (the 400 cannot be written either).
    serve_connection(
        &s,
        false,
        &flag,
        &mut Cursor::new(vec![0u8; 4]),
        &mut FailWriter,
    );
}

/// One request over an open connection; returns (status, headers, body).
fn roundtrip(conn: &mut TcpStream, raw: &str) -> (u16, String, Vec<u8>) {
    conn.write_all(raw.as_bytes()).unwrap();
    let mut reader = BufReader::new(conn.try_clone().unwrap());
    let mut head = String::new();
    loop {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "connection closed"
        );
        head.push_str(&line);
        if line == "\r\n" {
            break;
        }
    }
    let status = head[9..12].parse().unwrap();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .unwrap()
        .parse()
        .unwrap();
    let mut body = vec![0u8; if raw.starts_with("HEAD") { 0 } else { len }];
    reader.read_exact(&mut body).unwrap();
    (status, head, body)
}

fn start(dir: &Path) -> Server {
    Server::start(&ServeConfig {
        address: Some("127.0.0.1:0".into()),
        disk: Some(dir.to_path_buf()),
        verbose: false,
    })
    .unwrap()
}

#[test]
fn server_serves_and_shuts_down_with_idle_connections() {
    let dir = tempfile::tempdir().unwrap();
    let server = start(dir.path());
    let addr = server.addr();
    assert_eq!(addr.ip(), Ipv4Addr::LOCALHOST);
    assert_ne!(addr.port(), 0);

    let mut a = TcpStream::connect(addr).unwrap();
    let (status, head, body) = roundtrip(&mut a, "GET /v2/ HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_eq!((status, body.as_slice()), (200, &b"{}"[..]));
    assert!(head.contains("Docker-Distribution-API-Version: registry/2.0\r\n"));
    // Same connection again (keep-alive).
    let (status, _, _) = roundtrip(&mut a, "HEAD /v2/ HTTP/1.1\r\n\r\n");
    assert_eq!(status, 200);
    // A second connection that stays idle.
    let mut b = TcpStream::connect(addr).unwrap();
    assert_eq!(roundtrip(&mut b, "GET /v2/ HTTP/1.1\r\n\r\n").0, 200);

    // A second server on the same directory is refused.
    let err = Server::start(&ServeConfig {
        address: Some("127.0.0.1:0".into()),
        disk: Some(dir.path().to_path_buf()),
        verbose: false,
    })
    .err()
    .unwrap();
    assert!(err.to_string().contains("in use"), "{err}");

    // Shutdown returns although both connections are still open and idle, and
    // only after every thread has ended (nothing holds the shared state).
    let shared = Arc::downgrade(&server.shared);
    server.shutdown();
    assert!(shared.upgrade().is_none(), "a thread outlived shutdown");
    let mut rest = Vec::new();
    assert_eq!(a.read_to_end(&mut rest).unwrap(), 0);
    assert!(
        TcpStream::connect(addr).is_err() || {
            // Some systems accept briefly into a dead backlog; nothing is served.
            let mut c = TcpStream::connect(addr).unwrap();
            let _ = c.write_all(b"GET /v2/ HTTP/1.1\r\n\r\n");
            let mut buf = Vec::new();
            c.read_to_end(&mut buf).map(|n| n == 0).unwrap_or(true)
        }
    );

    // The lock is released: a new server can use the directory.
    let again = start(dir.path());
    again.shutdown();
}

#[test]
fn many_servers_start_and_stop() {
    for _ in 0..5 {
        let dir = tempfile::tempdir().unwrap();
        let server = start(dir.path());
        let mut c = TcpStream::connect(server.addr()).unwrap();
        assert_eq!(roundtrip(&mut c, "GET /v2/ HTTP/1.1\r\n\r\n").0, 200);
        drop(c);
        server.shutdown();
    }
}

#[test]
fn start_errors() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file");
    fs::write(&file, b"").unwrap();
    let cfg = |disk: &Path, address: &str| ServeConfig {
        address: Some(address.into()),
        disk: Some(disk.to_path_buf()),
        verbose: false,
    };
    let err = Server::start(&cfg(&file.join("sub"), "127.0.0.1:0"))
        .err()
        .unwrap();
    assert!(err.to_string().contains("cannot create"), "{err}");

    let bad_store = dir.path().join("bad");
    fs::create_dir_all(&bad_store).unwrap();
    fs::write(bad_store.join("blobs"), b"").unwrap();
    let err = Server::start(&cfg(&bad_store, "127.0.0.1:0"))
        .err()
        .unwrap();
    assert!(err.to_string().contains("cannot open storage"), "{err}");

    let ok = dir.path().join("ok");
    let err = Server::start(&cfg(&ok, "not an address")).err().unwrap();
    assert!(
        err.to_string().contains("cannot listen on not an address"),
        "{err}"
    );
    // The failed start released its lock.
    start(&ok).shutdown();
}

#[test]
fn serve_prints_address_and_blocks_until_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = ServeConfig {
        address: Some("127.0.0.1:0".into()),
        disk: Some(dir.path().to_path_buf()),
        verbose: true,
    };
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut err = Vec::new();
        let res = serve_until(&cfg, &mut err, &mut |stopper| tx.send(stopper).unwrap());
        (res, String::from_utf8(err).unwrap())
    });
    let stopper = rx.recv().unwrap();
    // Serving (verbose) while blocked.
    let mut c = TcpStream::connect(stopper.addr).unwrap();
    assert_eq!(roundtrip(&mut c, "GET /v2/ HTTP/1.1\r\n\r\n").0, 200);
    stopper.stop();
    let (res, err) = handle.join().unwrap();
    res.unwrap();
    assert_eq!(err, format!("listening on http://{}\n", stopper.addr));
}

#[test]
fn serve_fails_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = ServeConfig {
        address: Some("not an address".into()),
        disk: Some(dir.path().to_path_buf()),
        verbose: false,
    };
    assert!(serve(&cfg, &mut Vec::new()).is_err());
    // stderr unwritable: the server is shut down and the lock released.
    let cfg = ServeConfig {
        address: Some("127.0.0.1:0".into()),
        ..cfg
    };
    assert!(serve_until(&cfg, &mut FailWriter, &mut |_| unreachable!()).is_err());
    start(dir.path()).shutdown();
}
