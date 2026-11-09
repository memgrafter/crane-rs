//! `crane registry serve`: an OCI Distribution v2 registry on a directory.
//!
//! std::net, one thread per connection, HTTP/1.1 keep-alive. Storage layout:
//! `store.rs` (crane.ts `StorageLayout`); routes: `routes.rs` (crane.ts `Route`).

mod http;
mod routes;
mod store;

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use store::Store;

pub const DEFAULT_PORT: u16 = 5050;

/// Unread request-body bytes discarded to keep a connection alive; past this, close.
const DRAIN_LIMIT: u64 = 1 << 20;
/// A connection idle (or stalled mid-request) this long is closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// After closing our side, how long and how much unread input to discard, so the
/// peer reads our last response instead of a TCP reset.
const LINGER_TIME: Duration = Duration::from_secs(1);
const LINGER_BYTES: u64 = 1 << 20;

#[derive(Debug, Clone, Default)]
pub struct ServeConfig {
    /// "host:port"; None = "127.0.0.1:" + ($PORT or DEFAULT_PORT).
    pub address: Option<String>,
    /// Storage root; None = $HOME/.local/share/crane/registry.
    pub disk: Option<PathBuf>,
    /// Log one line per request to stderr.
    pub verbose: bool,
}

/// Storage root: `disk`, else `$HOME/.local/share/crane/registry`.
fn storage_root(disk: Option<&Path>, home: Option<std::ffi::OsString>) -> Result<PathBuf> {
    match (disk, home) {
        (Some(d), _) => Ok(d.to_path_buf()),
        (None, Some(h)) => Ok(PathBuf::from(h).join(".local/share/crane/registry")),
        (None, None) => bail!("no --disk given and $HOME is not set"),
    }
}

/// Listen address: `address`, else 127.0.0.1 on `$PORT` (if a valid port) or DEFAULT_PORT.
fn listen_address(address: Option<&str>, port_env: Option<&str>) -> String {
    match address {
        Some(a) => a.to_string(),
        None => {
            let port = port_env
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(DEFAULT_PORT);
            format!("127.0.0.1:{port}")
        }
    }
}

/// Where to connect to reach a listener bound to `addr` (wildcards become loopback).
fn wake_addr(mut addr: SocketAddr) -> SocketAddr {
    if addr.ip().is_unspecified() {
        addr.set_ip(match addr {
            SocketAddr::V4(_) => Ipv4Addr::LOCALHOST.into(),
            SocketAddr::V6(_) => Ipv6Addr::LOCALHOST.into(),
        });
    }
    addr
}

/// Take `serve.lock` under `root`; fails if another server holds it.
fn lock_storage(root: &Path) -> Result<File> {
    let path = root.join("serve.lock");
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    file.try_lock().map_err(|e| {
        anyhow!(
            "{} is in use by another registry server ({}: {e})",
            root.display(),
            path.display()
        )
    })?;
    Ok(file)
}

struct Shared {
    store: Store,
    verbose: bool,
    stop: AtomicBool,
    conns: Mutex<Conns>,
}

#[derive(Default)]
struct Conns {
    next: u64,
    open: HashMap<u64, Arc<TcpStream>>,
    threads: Vec<JoinHandle<()>>,
}

impl Shared {
    fn conns(&self) -> MutexGuard<'_, Conns> {
        self.conns.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A running registry. Dropping it does not stop it; call `shutdown`.
pub struct Server {
    addr: SocketAddr,
    shared: Arc<Shared>,
    accept: JoinHandle<()>,
    _lock: File,
}

/// Stops a server from another thread.
struct Stopper {
    shared: Arc<Shared>,
    addr: SocketAddr,
}

impl Stopper {
    fn stop(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect(wake_addr(self.addr));
    }
}

impl Server {
    /// Bind, take the storage lock, start accepting on background threads.
    pub fn start(cfg: &ServeConfig) -> Result<Server> {
        let root = storage_root(cfg.disk.as_deref(), std::env::var_os("HOME"))?;
        fs::create_dir_all(&root).with_context(|| format!("cannot create {}", root.display()))?;
        let lock = lock_storage(&root)?;
        let store = Store::open(&root)
            .with_context(|| format!("cannot open storage in {}", root.display()))?;
        let port_env = std::env::var("PORT").ok();
        let address = listen_address(cfg.address.as_deref(), port_env.as_deref());
        let listener =
            TcpListener::bind(&address).with_context(|| format!("cannot listen on {address}"))?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            store,
            verbose: cfg.verbose,
            stop: AtomicBool::new(false),
            conns: Mutex::new(Conns::default()),
        });
        let s = Arc::clone(&shared);
        let accept = thread::spawn(move || accept_loop(&s, &listener));
        Ok(Server {
            addr,
            shared,
            accept,
            _lock: lock,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    fn stopper(&self) -> Stopper {
        Stopper {
            shared: Arc::clone(&self.shared),
            addr: self.addr,
        }
    }

    /// Stop accepting, finish in-flight requests, release the lock.
    pub fn shutdown(self) {
        self.stopper().stop();
        self.wait();
    }

    /// Block until stopped; then close idle connections, join every thread and
    /// release the lock.
    fn wait(self) {
        let _ = self.accept.join();
        let (open, threads) = {
            let mut c = self.shared.conns();
            (std::mem::take(&mut c.open), std::mem::take(&mut c.threads))
        };
        // A connection waiting for its next request sees EOF; one mid-response finishes.
        for stream in open.values() {
            let _ = stream.shutdown(Shutdown::Read);
        }
        for t in threads {
            let _ = t.join();
        }
    }
}

fn accept_loop(shared: &Arc<Shared>, listener: &TcpListener) {
    for stream in listener.incoming() {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        if let Ok(stream) = stream {
            spawn_connection(shared, stream);
        }
    }
}

fn spawn_connection(shared: &Arc<Shared>, stream: TcpStream) {
    let stream = Arc::new(stream);
    let mut conns = shared.conns();
    let id = conns.next;
    conns.next += 1;
    conns.open.insert(id, Arc::clone(&stream));
    conns.threads.retain(|t| !t.is_finished());
    let s = Arc::clone(shared);
    conns.threads.push(thread::spawn(move || {
        let _ = stream.set_read_timeout(Some(IDLE_TIMEOUT));
        let _ = stream.set_nodelay(true);
        serve_connection(
            &s.store,
            s.verbose,
            &s.stop,
            &mut BufReader::new(&*stream),
            &mut &*stream,
        );
        // Lingering close: send FIN, then drain what the peer is still sending.
        let _ = stream.shutdown(Shutdown::Write);
        let _ = stream.set_read_timeout(Some(LINGER_TIME));
        let _ = io::copy(&mut (&*stream).take(LINGER_BYTES), &mut io::sink());
        s.conns().open.remove(&id);
    }));
}

/// Serve requests from one connection until it closes, errs, or asks to close.
fn serve_connection(
    store: &Store,
    verbose: bool,
    stop: &AtomicBool,
    reader: &mut dyn BufRead,
    writer: &mut dyn Write,
) {
    loop {
        let req = match http::read_head(reader) {
            Ok(Some(req)) => req,
            Ok(None) => return,
            Err(e) => {
                if e.kind() == io::ErrorKind::InvalidData {
                    let resp = routes::err(400, "UNSUPPORTED", format!("bad HTTP request: {e}"));
                    let _ = resp.into_response().write_to(writer, false, true);
                }
                return;
            }
        };
        if req.expects_continue()
            && writer
                .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                .and_then(|()| writer.flush())
                .is_err()
        {
            return;
        }
        let mut body = http::Body::new(reader, &req.framing);
        let mut resp = routes::handle(store, &req, &mut body);
        if body.failed() {
            resp = routes::err(
                400,
                "BLOB_UPLOAD_INVALID",
                "malformed or truncated request body",
            )
            .into_response();
        }
        let close = !body.drain(DRAIN_LIMIT) || req.wants_close() || stop.load(Ordering::SeqCst);
        if verbose {
            eprintln!("{} {} {}", req.method, req.path, resp.status);
        }
        if resp.write_to(writer, req.method == "HEAD", close).is_err() || close {
            return;
        }
    }
}

/// Start, print "listening on http://<addr>" to `err`, then block until stopped.
/// Stops cleanly on SIGINT / SIGTERM (unix).
pub fn serve(cfg: &ServeConfig, err: &mut dyn Write) -> Result<()> {
    #[cfg(unix)]
    {
        let set = signals::block();
        serve_until(cfg, err, &mut |stopper| {
            signals::stop_on_signal(stopper, set);
        })
    }
    #[cfg(not(unix))]
    serve_until(cfg, err, &mut drop)
}

/// `serve`, handing a `Stopper` to `on_start` (tests use it to stop the server).
fn serve_until(
    cfg: &ServeConfig,
    err: &mut dyn Write,
    on_start: &mut dyn FnMut(Stopper),
) -> Result<()> {
    let server = Server::start(cfg)?;
    if let Err(e) =
        writeln!(err, "listening on http://{}", server.addr()).and_then(|()| err.flush())
    {
        server.shutdown();
        return Err(e.into());
    }
    on_start(server.stopper());
    server.wait();
    Ok(())
}

#[cfg(unix)]
mod signals;

#[cfg(test)]
mod tests;
