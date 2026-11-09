//! End-to-end tests of `crane registry serve`: the real binary on 127.0.0.1:0,
//! driven over raw HTTP and (when present) by go crane as the client.
//!
//! go crane: $CRANE_GO, default the pinned v0.22.1 oracle below; its tests are
//! skipped with a message when it is missing.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crane::digest;
use serde_json::{Value, json};

const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const DEFAULT_GO_CRANE: &str = "/Volumes/ExternalData/code/crane-rs/.tools/bin/crane";

// ---------------------------------------------------------------------------
// Harness

/// A running `crane registry serve`; killed on drop.
struct Registry {
    child: Child,
    addr: String,
    stderr: Arc<Mutex<String>>,
}

impl Registry {
    fn start(disk: &Path) -> Registry {
        Self::spawn(
            &["registry", "serve", "--address", "127.0.0.1:0", "--disk"],
            Some(disk),
            &[],
        )
    }

    fn spawn(args: &[&str], disk: Option<&Path>, env: &[(&str, &Path)]) -> Registry {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_crane"));
        cmd.args(args);
        if let Some(d) = disk {
            cmd.arg(d);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let mut first = String::new();
        stderr.read_line(&mut first).unwrap();
        let addr = first
            .trim()
            .strip_prefix("listening on http://")
            .unwrap_or_else(|| panic!("unexpected first stderr line: {first:?}"))
            .to_string();
        // Keep draining stderr (verbose request logs) so the server never blocks on it.
        let log = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&log);
        thread::spawn(move || {
            for line in stderr.lines() {
                let Ok(line) = line else { break };
                let mut s = sink.lock().unwrap();
                s.push_str(&line);
                s.push('\n');
            }
        });
        Registry {
            child,
            addr,
            stderr: log,
        }
    }

    fn get(&self, path: &str) -> Resp {
        req(&self.addr, "GET", path, &[], b"")
    }
}

impl Drop for Registry {
    /// SIGTERM so the server exits cleanly (and an instrumented binary writes its
    /// coverage data); SIGKILL if it has not exited within 5 s.
    fn drop(&mut self) {
        // SAFETY: plain kill(2) on our own child's pid.
        unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.child.try_wait().ok().flatten().is_none() {
            if std::time::Instant::now() > deadline {
                let _ = self.child.kill();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = self.child.wait();
    }
}

#[derive(Debug)]
struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn h(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&self.body)))
    }
    /// The first error code of an error body.
    fn code(&self) -> String {
        self.json()["errors"][0]["code"]
            .as_str()
            .unwrap()
            .to_string()
    }
    /// (status, error code)
    fn err(&self) -> (u16, String) {
        (self.status, self.code())
    }
}

/// Parse one response from the front of `raw`; returns it and the rest.
fn parse_resp(raw: &[u8], head_request: bool) -> (Resp, &[u8]) {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no header end in {:?}", String::from_utf8_lossy(raw)));
    let head = std::str::from_utf8(&raw[..end]).unwrap();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap();
    assert!(status_line.starts_with("HTTP/1.1 "), "{status_line}");
    let status = status_line[9..12].parse().unwrap();
    let headers: Vec<(String, String)> = lines
        .map(|l| {
            let (n, v) = l.split_once(": ").unwrap();
            (n.to_string(), v.to_string())
        })
        .collect();
    let resp = Resp {
        status,
        headers,
        body: Vec::new(),
    };
    let len: usize = resp.h("content-length").unwrap_or("0").parse().unwrap();
    let start = end + 4;
    let body_len = if head_request || status == 100 {
        0
    } else {
        len
    };
    let body = raw[start..start + body_len].to_vec();
    (Resp { body, ..resp }, &raw[start + body_len..])
}

/// Write `raw` on a fresh connection and read until the server closes it.
fn exchange(addr: &str, raw: &[u8]) -> Vec<u8> {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(raw).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    out
}

fn request_bytes(method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut raw = format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
    let chunked = headers
        .iter()
        .any(|(n, _)| n.eq_ignore_ascii_case("transfer-encoding"));
    for (n, v) in headers {
        raw.push_str(&format!("{n}: {v}\r\n"));
    }
    if !chunked {
        raw.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    raw.push_str("\r\n");
    let mut raw = raw.into_bytes();
    raw.extend_from_slice(body);
    raw
}

/// One request with `Connection: close` and a Content-Length body.
fn req(addr: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Resp {
    let raw = exchange(addr, &request_bytes(method, path, headers, body));
    let (resp, rest) = parse_resp(&raw, method == "HEAD");
    assert!(rest.is_empty(), "trailing bytes: {rest:?}");
    assert_eq!(
        resp.h("docker-distribution-api-version"),
        Some("registry/2.0"),
        "{method} {path}"
    );
    resp
}

fn push_blob(addr: &str, repo: &str, bytes: &[u8]) -> String {
    let d = digest::of(bytes);
    let r = req(
        addr,
        "POST",
        &format!("/v2/{repo}/blobs/uploads/?digest={d}"),
        &[],
        bytes,
    );
    assert_eq!(r.status, 201);
    d
}

fn put_manifest(addr: &str, repo: &str, reference: &str, ct: &str, body: &[u8]) -> Resp {
    req(
        addr,
        "PUT",
        &format!("/v2/{repo}/manifests/{reference}"),
        &[("Content-Type", ct)],
        body,
    )
}

fn image_manifest(config: &str, layers: &[&str]) -> String {
    let layers: Vec<Value> = layers
        .iter()
        .map(|d| json!({"mediaType": "application/vnd.oci.image.layer.v1.tar", "size": 1, "digest": d}))
        .collect();
    json!({
        "schemaVersion": 2,
        "mediaType": OCI_MANIFEST,
        "config": {"mediaType": OCI_CONFIG, "size": 2, "digest": config},
        "layers": layers,
    })
    .to_string()
}

/// Push a config + one layer + manifest tagged `tag`; returns (manifest, digest).
fn push_image(addr: &str, repo: &str, tag: &str, layer: &[u8]) -> (String, String) {
    let config = push_blob(addr, repo, b"{}");
    let layer = push_blob(addr, repo, layer);
    let m = image_manifest(&config, &[&layer]);
    let r = put_manifest(addr, repo, tag, OCI_MANIFEST, m.as_bytes());
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let d = digest::of(m.as_bytes());
    (m, d)
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn s(x: &str) -> String {
    x.to_string()
}

// ---------------------------------------------------------------------------
// Raw HTTP

#[test]
fn base_route_and_unsupported() {
    let dir = tmp();
    let r = Registry::start(dir.path());
    let g = r.get("/v2/");
    assert_eq!((g.status, g.body.as_slice()), (200, &b"{}"[..]));
    assert_eq!(g.h("content-type"), Some("application/json"));
    let h = req(&r.addr, "HEAD", "/v2/", &[], b"");
    assert_eq!((h.status, h.h("content-length")), (200, Some("2")));
    assert!(h.body.is_empty());
    assert_eq!(r.get("/v2").status, 200);

    for (method, path) in [
        ("GET", "/"),
        ("GET", "/v1/"),
        ("GET", "/v2/a/unknown/x"),
        ("POST", "/v2/"),
        ("OPTIONS", "/v2/"),
        ("DELETE", "/v2/_catalog"),
        ("PATCH", "/v2/a/manifests/latest"),
        ("PUT", "/v2/a/tags/list"),
    ] {
        let g = req(&r.addr, method, path, &[], b"");
        assert_eq!(g.err(), (405, s("UNSUPPORTED")), "{method} {path}");
    }
}

#[test]
fn monolithic_and_chunked_uploads() {
    let dir = tmp();
    let r = Registry::start(dir.path());
    let a = r.addr.as_str();

    // Monolithic.
    let d = digest::of(b"one shot");
    let g = req(
        a,
        "POST",
        &format!("/v2/x/y/blobs/uploads/?digest={d}"),
        &[],
        b"one shot",
    );
    assert_eq!(g.status, 201);
    assert_eq!(g.h("location"), Some(format!("/v2/x/y/blobs/{d}").as_str()));
    assert_eq!(g.h("docker-content-digest"), Some(d.as_str()));
    assert_eq!(r.get(&format!("/v2/x/y/blobs/{d}")).body, b"one shot");

    // Chunked: POST, PATCH (with and without Content-Range, chunked transfer), status, PUT.
    let g = req(a, "POST", "/v2/x/y/blobs/uploads/", &[], b"");
    assert_eq!((g.status, g.h("range")), (202, Some("0-0")));
    let loc = g.h("location").unwrap().to_string();
    let uuid = g.h("docker-upload-uuid").unwrap().to_string();
    assert!(loc.ends_with(&uuid));

    let g = req(
        a,
        "PATCH",
        &loc,
        &[("Content-Type", "application/octet-stream")],
        b"abc",
    );
    assert_eq!((g.status, g.h("range")), (202, Some("0-2")));
    assert_eq!(g.h("location"), Some(loc.as_str()));
    assert_eq!(g.h("docker-upload-uuid"), Some(uuid.as_str()));

    let g = req(a, "PATCH", &loc, &[("Content-Range", "3-5")], b"def");
    assert_eq!((g.status, g.h("range")), (202, Some("0-5")));

    let g = req(a, "PATCH", &loc, &[("Content-Range", "0-2")], b"xyz");
    assert_eq!(g.err(), (416, s("BLOB_UPLOAD_INVALID")));

    let g = req(
        a,
        "PATCH",
        &loc,
        &[("Transfer-Encoding", "chunked")],
        b"2\r\ngh\r\n1\r\ni\r\n0\r\n\r\n",
    );
    assert_eq!((g.status, g.h("range")), (202, Some("0-8")));

    let g = req(a, "GET", &loc, &[], b"");
    assert_eq!((g.status, g.h("range")), (204, Some("0-8")));

    let g = req(a, "PUT", &loc, &[], b"");
    assert_eq!(g.err(), (400, s("DIGEST_INVALID")));

    let d = digest::of(b"abcdefghij");
    let g = req(a, "PUT", &format!("{loc}?digest={d}"), &[], b"j");
    assert_eq!(g.status, 201);
    assert_eq!(g.h("docker-content-digest"), Some(d.as_str()));
    assert_eq!(r.get(&format!("/v2/x/y/blobs/{d}")).body, b"abcdefghij");
    let g = req(a, "GET", &loc, &[], b"");
    assert_eq!(g.err(), (404, s("BLOB_UPLOAD_UNKNOWN")));

    // Digest mismatch at the end discards the upload.
    let loc = req(a, "POST", "/v2/x/y/blobs/uploads/", &[], b"")
        .h("location")
        .unwrap()
        .to_string();
    let g = req(a, "PUT", &format!("{loc}?digest={d}"), &[], b"wrong");
    assert_eq!(g.err(), (400, s("DIGEST_INVALID")));
    assert_eq!(req(a, "GET", &loc, &[], b"").status, 404);

    // Cancel.
    let loc = req(a, "POST", "/v2/x/y/blobs/uploads/", &[], b"")
        .h("location")
        .unwrap()
        .to_string();
    assert_eq!(req(a, "DELETE", &loc, &[], b"").status, 204);
    let g = req(a, "DELETE", &loc, &[], b"");
    assert_eq!(g.err(), (404, s("BLOB_UPLOAD_UNKNOWN")));
    // Path traversal in the name is NAME_INVALID; in the upload id, not a route.
    let g = req(a, "POST", "/v2/../../etc/blobs/uploads/", &[], b"");
    assert_eq!(g.err(), (400, s("NAME_INVALID")));
    let g = req(a, "PATCH", "/v2/x/y/blobs/uploads/../../etc", &[], b"");
    assert_eq!(g.err(), (405, s("UNSUPPORTED")));
    let g = req(a, "PATCH", "/v2/x/y/blobs/uploads/..%2F..%2Fetc", &[], b"");
    assert_eq!(g.err(), (405, s("UNSUPPORTED")));

    // A malformed chunked body: 400, connection closed.
    let raw = request_bytes(
        "POST",
        &format!("/v2/x/blobs/uploads/?digest={d}"),
        &[("Transfer-Encoding", "chunked")],
        b"not-hex\r\n",
    );
    let out = exchange(a, &raw);
    let (g, _) = parse_resp(&out, false);
    assert_eq!(g.err(), (400, s("BLOB_UPLOAD_INVALID")));

    // Unsupported digest algorithm.
    let g = req(
        a,
        "POST",
        "/v2/x/blobs/uploads/?digest=sha512:abcd",
        &[],
        b"",
    );
    assert_eq!(g.err(), (400, s("UNSUPPORTED")));
}

#[test]
fn blob_reads_ranges_and_delete() {
    let dir = tmp();
    let r = Registry::start(dir.path());
    let a = r.addr.as_str();
    let d = push_blob(a, "r", b"0123456789");

    let g = r.get(&format!("/v2/r/blobs/{d}"));
    assert_eq!((g.status, g.body.as_slice()), (200, &b"0123456789"[..]));
    assert_eq!(g.h("content-type"), Some("application/octet-stream"));
    assert_eq!(g.h("docker-content-digest"), Some(d.as_str()));
    let h = req(a, "HEAD", &format!("/v2/r/blobs/{d}"), &[], b"");
    assert_eq!((h.status, h.h("content-length")), (200, Some("10")));

    let g = req(
        a,
        "GET",
        &format!("/v2/r/blobs/{d}"),
        &[("Range", "bytes=2-4")],
        b"",
    );
    assert_eq!((g.status, g.body.as_slice()), (206, &b"234"[..]));
    assert_eq!(g.h("content-range"), Some("bytes 2-4/10"));
    let g = req(
        a,
        "GET",
        &format!("/v2/r/blobs/{d}"),
        &[("Range", "bytes=-3")],
        b"",
    );
    assert_eq!((g.status, g.body.as_slice()), (206, &b"789"[..]));
    let g = req(
        a,
        "GET",
        &format!("/v2/r/blobs/{d}"),
        &[("Range", "bytes=20-")],
        b"",
    );
    assert_eq!(g.status, 416);
    assert_eq!(g.h("content-range"), Some("bytes */10"));

    let g = r.get(&format!("/v2/other/blobs/{d}"));
    assert_eq!(g.err(), (404, s("BLOB_UNKNOWN")));
    let g = r.get("/v2/r/blobs/sha256:short");
    assert_eq!(g.err(), (400, s("DIGEST_INVALID")));
    let g = r.get(&format!("/v2/Bad/blobs/{d}"));
    assert_eq!(g.err(), (400, s("NAME_INVALID")));

    assert_eq!(
        req(a, "DELETE", &format!("/v2/r/blobs/{d}"), &[], b"").status,
        202
    );
    let g = req(a, "DELETE", &format!("/v2/r/blobs/{d}"), &[], b"");
    assert_eq!(g.err(), (404, s("BLOB_UNKNOWN")));
    assert_eq!(r.get(&format!("/v2/r/blobs/{d}")).status, 404);
    // The bytes stay for other repos that link them.
    assert!(dir.path().join("blobs/sha256").join(&d[7..]).is_file());
}

#[test]
fn cross_repo_mount() {
    let dir = tmp();
    let r = Registry::start(dir.path());
    let a = r.addr.as_str();
    let d = push_blob(a, "src/repo", b"shared");
    let g = req(
        a,
        "POST",
        &format!("/v2/dst/blobs/uploads/?mount={d}&from=src/repo"),
        &[],
        b"",
    );
    assert_eq!(g.status, 201);
    assert_eq!(g.h("location"), Some(format!("/v2/dst/blobs/{d}").as_str()));
    assert_eq!(r.get(&format!("/v2/dst/blobs/{d}")).body, b"shared");

    // Not in the source repo: falls back to a normal upload.
    let g = req(
        a,
        "POST",
        &format!("/v2/dst2/blobs/uploads/?mount={d}&from=nope"),
        &[],
        b"",
    );
    assert_eq!(g.status, 202);
    assert!(g.h("docker-upload-uuid").is_some());
    assert_eq!(r.get(&format!("/v2/dst2/blobs/{d}")).status, 404);
}

#[test]
fn manifests_and_errors() {
    let dir = tmp();
    let r = Registry::start(dir.path());
    let a = r.addr.as_str();
    let (m, d) = push_image(a, "app/web", "v1", b"L");

    for reference in ["v1", d.as_str()] {
        let g = r.get(&format!("/v2/app/web/manifests/{reference}"));
        assert_eq!(g.status, 200);
        assert_eq!(g.body, m.as_bytes());
        assert_eq!(g.h("content-type"), Some(OCI_MANIFEST));
        assert_eq!(g.h("docker-content-digest"), Some(d.as_str()));
    }
    let h = req(a, "HEAD", "/v2/app/web/manifests/v1", &[], b"");
    assert_eq!(h.h("content-length"), Some(m.len().to_string().as_str()));

    // PUT by digest: Location and digest headers; must match the body.
    let g = put_manifest(a, "app/web", &d, OCI_MANIFEST, m.as_bytes());
    assert_eq!(g.status, 201);
    assert_eq!(
        g.h("location"),
        Some(format!("/v2/app/web/manifests/{d}").as_str())
    );
    let other = digest::of(b"other");
    let g = put_manifest(a, "app/web", &other, OCI_MANIFEST, m.as_bytes());
    assert_eq!(g.err(), (400, s("DIGEST_INVALID")));

    // Validation errors.
    let g = put_manifest(a, "app/web", "t", OCI_MANIFEST, b"{not json");
    assert_eq!(g.err(), (400, s("MANIFEST_INVALID")));
    let g = req(
        a,
        "PUT",
        "/v2/app/web/manifests/t",
        &[],
        b"{\"schemaVersion\":2}",
    );
    assert_eq!(g.err(), (400, s("MANIFEST_INVALID")));
    let missing = image_manifest(&other, &[]);
    let g = put_manifest(a, "app/web", "t", OCI_MANIFEST, missing.as_bytes());
    assert_eq!(g.err(), (400, s("MANIFEST_BLOB_UNKNOWN")));
    let idx = json!({"schemaVersion": 2, "mediaType": OCI_INDEX,
        "manifests": [{"mediaType": OCI_MANIFEST, "size": 1, "digest": other}]})
    .to_string();
    let g = put_manifest(a, "app/web", "t", OCI_INDEX, idx.as_bytes());
    assert_eq!(g.err(), (400, s("MANIFEST_BLOB_UNKNOWN")));
    let g = put_manifest(a, "app/web", "-bad", OCI_MANIFEST, m.as_bytes());
    assert_eq!(g.err(), (400, s("TAG_INVALID")));
    let g = put_manifest(a, "App", "t", OCI_MANIFEST, m.as_bytes());
    assert_eq!(g.err(), (400, s("NAME_INVALID")));
    let huge = format!("{{\"x\":\"{}\"}}", "a".repeat(4 * 1024 * 1024));
    let g = put_manifest(a, "app/web", "t", OCI_MANIFEST, huge.as_bytes());
    assert_eq!(g.err(), (413, s("SIZE_INVALID")));

    // An index of the image; mediaType from the body when there is no Content-Type.
    let idx = json!({"schemaVersion": 2, "mediaType": OCI_INDEX,
        "manifests": [{"mediaType": OCI_MANIFEST, "size": m.len(), "digest": d}]})
    .to_string();
    let g = req(a, "PUT", "/v2/app/web/manifests/idx", &[], idx.as_bytes());
    assert_eq!(g.status, 201);
    assert_eq!(
        r.get("/v2/app/web/manifests/idx").h("content-type"),
        Some(OCI_INDEX)
    );

    // Unknown.
    let g = r.get("/v2/app/web/manifests/nope");
    assert_eq!(g.err(), (404, s("MANIFEST_UNKNOWN")));
    let g = r.get(&format!("/v2/app/web/manifests/{other}"));
    assert_eq!(g.err(), (404, s("MANIFEST_UNKNOWN")));

    // Delete by tag removes only the tag; by digest removes it and its tags.
    put_manifest(a, "app/web", "v2", OCI_MANIFEST, m.as_bytes());
    assert_eq!(
        req(a, "DELETE", "/v2/app/web/manifests/v2", &[], b"").status,
        202
    );
    assert_eq!(r.get("/v2/app/web/manifests/v2").status, 404);
    assert_eq!(r.get("/v2/app/web/manifests/v1").status, 200);
    let g = req(a, "DELETE", &format!("/v2/app/web/manifests/{d}"), &[], b"");
    assert_eq!(g.status, 202);
    assert_eq!(r.get("/v2/app/web/manifests/v1").status, 404);
    let g = req(a, "DELETE", &format!("/v2/app/web/manifests/{d}"), &[], b"");
    assert_eq!(g.err(), (404, s("MANIFEST_UNKNOWN")));
    let tags = r.get("/v2/app/web/tags/list").json();
    assert_eq!(tags["tags"], json!(["idx"]));
}

#[test]
fn referrers() {
    let dir = tmp();
    let r = Registry::start(dir.path());
    let a = r.addr.as_str();
    let (m, subject) = push_image(a, "r", "v1", b"L");
    let empty = push_blob(a, "r", b"{}");
    let sig = json!({
        "schemaVersion": 2, "mediaType": OCI_MANIFEST, "artifactType": "application/x.sig",
        "config": {"mediaType": "application/vnd.oci.empty.v1+json", "size": 2, "digest": empty},
        "layers": [],
        "subject": {"mediaType": OCI_MANIFEST, "size": m.len(), "digest": subject},
        "annotations": {"org.example": "yes"},
    })
    .to_string();
    let sig_d = digest::of(sig.as_bytes());
    let g = put_manifest(a, "r", &sig_d, OCI_MANIFEST, sig.as_bytes());
    assert_eq!(g.status, 201);
    assert_eq!(g.h("oci-subject"), Some(subject.as_str()));

    let g = r.get(&format!("/v2/r/referrers/{subject}"));
    assert_eq!(g.status, 200);
    assert_eq!(g.h("content-type"), Some(OCI_INDEX));
    assert_eq!(
        g.json(),
        json!({"schemaVersion": 2, "mediaType": OCI_INDEX, "manifests": [{
            "mediaType": OCI_MANIFEST, "digest": sig_d, "size": sig.len(),
            "artifactType": "application/x.sig", "annotations": {"org.example": "yes"}}]})
    );
    let g = r.get(&format!(
        "/v2/r/referrers/{subject}?artifactType=application/x.other"
    ));
    assert_eq!(g.h("oci-filters-applied"), Some("artifactType"));
    assert_eq!(g.json()["manifests"], json!([]));
    let g = r.get(&format!("/v2/r/referrers/{sig_d}"));
    assert_eq!(g.json()["manifests"], json!([]));
    let g = r.get("/v2/r/referrers/sha256:nope");
    assert_eq!(g.err(), (400, s("DIGEST_INVALID")));
}

#[test]
fn tags_and_catalog_paging() {
    let dir = tmp();
    let r = Registry::start(dir.path());
    let a = r.addr.as_str();
    let g = r.get("/v2/r/tags/list");
    assert_eq!(g.err(), (404, s("NAME_UNKNOWN")));
    let (m, _) = push_image(a, "r", "c", b"L");
    for tag in ["b", "a", "d"] {
        assert_eq!(
            put_manifest(a, "r", tag, OCI_MANIFEST, m.as_bytes()).status,
            201
        );
    }
    let g = r.get("/v2/r/tags/list");
    assert_eq!(g.json(), json!({"name": "r", "tags": ["a", "b", "c", "d"]}));
    assert_eq!(g.h("link"), None);
    let g = r.get("/v2/r/tags/list?n=3");
    assert_eq!(g.json()["tags"], json!(["a", "b", "c"]));
    assert_eq!(
        g.h("link"),
        Some(r#"</v2/r/tags/list?n=3&last=c>; rel="next""#)
    );
    let g = r.get("/v2/r/tags/list?n=3&last=c");
    assert_eq!(
        (g.json()["tags"].clone(), g.h("link")),
        (json!(["d"]), None)
    );

    push_blob(a, "z/nested/repo", b"x");
    push_blob(a, "m", b"x");
    let g = r.get("/v2/_catalog");
    assert_eq!(
        g.json(),
        json!({"repositories": ["m", "r", "z/nested/repo"]})
    );
    let g = r.get("/v2/_catalog?n=2");
    assert_eq!(g.json(), json!({"repositories": ["m", "r"]}));
    assert_eq!(
        g.h("link"),
        Some(r#"</v2/_catalog?n=2&last=r>; rel="next""#)
    );
    let g = r.get("/v2/_catalog?n=2&last=r");
    assert_eq!(g.json(), json!({"repositories": ["z/nested/repo"]}));
}

#[test]
fn keep_alive_garbage_and_expect_continue() {
    let dir = tmp();
    let r = Registry::start(dir.path());
    let a = r.addr.as_str();

    // A TLS ClientHello on the plain port: 400 and close; the server keeps serving.
    let hello = [
        0x16, 0x03, 0x01, 0x00, 0x2f, 0x01, 0x00, 0x00, 0x2b, 0x03, 0x03, 0x00, 0x01, 0x02,
    ];
    let out = exchange(a, &hello);
    let (g, rest) = parse_resp(&out, false);
    assert_eq!(g.err(), (400, s("UNSUPPORTED")));
    assert_eq!(g.h("connection"), Some("close"));
    assert!(rest.is_empty());
    // Other junk: binary zeros, an absurd line, a peer that hangs up mid-head.
    for junk in [
        vec![0u8; 64],
        format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(20_000)).into_bytes(),
        b"GET / HTTP/1.1\r\nno colon\r\n\r\n".to_vec(),
    ] {
        let out = exchange(a, &junk);
        assert_eq!(parse_resp(&out, false).0.status, 400);
    }
    drop(TcpStream::connect(a).unwrap().write_all(b"GET /v2/ HT"));
    assert_eq!(r.get("/v2/").status, 200);

    // Three pipelined requests on one connection; the last asks to close.
    let d = digest::of(b"hi");
    let mut raw = format!("GET /v2/ HTTP/1.1\r\nHost: t\r\n\r\nPOST /v2/k/blobs/uploads/?digest={d} HTTP/1.1\r\nContent-Length: 2\r\n\r\nhi").into_bytes();
    raw.extend_from_slice(
        format!("HEAD /v2/k/blobs/{d} HTTP/1.1\r\nConnection: close\r\n\r\n").as_bytes(),
    );
    let out = exchange(a, &raw);
    let (g1, rest) = parse_resp(&out, false);
    let (g2, rest) = parse_resp(rest, false);
    let (g3, rest) = parse_resp(rest, true);
    assert_eq!((g1.status, g2.status, g3.status), (200, 201, 200));
    assert_eq!(g3.h("content-length"), Some("2"));
    assert!(rest.is_empty());

    // Expect: 100-continue gets an interim response.
    let d = digest::of(b"later");
    let raw = format!(
        "POST /v2/k/blobs/uploads/?digest={d} HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 5\r\nConnection: close\r\n\r\nlater"
    );
    let out = exchange(a, raw.as_bytes());
    let (g1, rest) = parse_resp(&out, false);
    let (g2, _) = parse_resp(rest, false);
    assert_eq!((g1.status, g2.status), (100, 201));
}

#[test]
fn second_server_on_same_dir_is_refused_and_data_persists() {
    let dir = tmp();
    let (m, d) = {
        let r = Registry::start(dir.path());
        let out = Command::new(env!("CARGO_BIN_EXE_crane"))
            .args(["registry", "serve", "--address", "127.0.0.1:0", "--disk"])
            .arg(dir.path())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("in use by another registry server"), "{err}");
        push_image(&r.addr, "keep", "v1", b"L")
    };
    // Restart on the same directory: everything is still there.
    let r = Registry::start(dir.path());
    let g = r.get("/v2/keep/manifests/v1");
    assert_eq!((g.status, g.body.as_slice()), (200, m.as_bytes()));
    assert_eq!(g.h("docker-content-digest"), Some(d.as_str()));
    // Plain files on disk, as crane.ts StorageLayout says.
    let repo = dir.path().join("repos/keep");
    assert!(repo.join("_manifests").join(&d[7..]).is_file());
    assert_eq!(
        std::fs::read_to_string(repo.join("_manifests").join(format!("{}.type", &d[7..]))).unwrap(),
        OCI_MANIFEST
    );
    assert_eq!(std::fs::read_to_string(repo.join("_tags/v1")).unwrap(), d);
    assert!(dir.path().join("serve.lock").is_file());
}

#[test]
fn defaults_from_port_and_home_and_verbose_log() {
    let home = tmp();
    // No --address and no --disk: 127.0.0.1:$PORT and $HOME/.local/share/crane/registry.
    let mut cmd_env = vec![("HOME", home.path())];
    let port_zero = PathBuf::from("0");
    cmd_env.push(("PORT", port_zero.as_path()));
    let r = Registry::spawn(&["-v", "registry", "serve"], None, &cmd_env);
    assert!(r.addr.starts_with("127.0.0.1:"), "{}", r.addr);
    assert_ne!(r.addr, "127.0.0.1:0");
    assert_eq!(r.get("/v2/").status, 200);
    assert_eq!(r.get("/v2/nope/tags/list").status, 404);
    assert!(
        home.path()
            .join(".local/share/crane/registry/serve.lock")
            .is_file()
    );
    // Verbose: one stderr line per request.
    for _ in 0..100 {
        let log = r.stderr.lock().unwrap().clone();
        if log.contains("GET /v2/nope/tags/list 404") {
            assert!(log.contains("GET /v2/ 200"), "{log}");
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("no verbose log line: {:?}", r.stderr.lock().unwrap());
}

// ---------------------------------------------------------------------------
// go crane as the client

fn go_crane() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var("CRANE_GO").unwrap_or_else(|_| DEFAULT_GO_CRANE.into()));
    if path.is_file() {
        Some(path)
    } else {
        eprintln!(
            "SKIP: go crane not found at {} (set CRANE_GO to the pinned v0.22.1 binary)",
            path.display()
        );
        None
    }
}

struct Go {
    bin: PathBuf,
    /// Empty DOCKER_CONFIG and HOME: no credentials, nothing from the user.
    home: tempfile::TempDir,
}

impl Go {
    fn run(&self, args: &[&str]) -> Output {
        Command::new(&self.bin)
            .args(args)
            .arg("--insecure")
            .env("DOCKER_CONFIG", self.home.path())
            .env("HOME", self.home.path())
            .env_remove("HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("http_proxy")
            .env_remove("https_proxy")
            .current_dir(self.home.path())
            .output()
            .unwrap()
    }

    /// Run, require success, return stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "go crane {args:?} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
}

/// A deterministic layer tarball: fixed names, modes, owners and mtimes.
fn layer_tar(path: &Path, files: &[(&str, &[u8])]) {
    let mut b = tar::Builder::new(Vec::new());
    for (name, data) in files {
        let mut h = tar::Header::new_ustar();
        h.set_path(name).unwrap();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        b.append(&h, *data).unwrap();
    }
    std::fs::write(path, b.into_inner().unwrap()).unwrap();
}

#[test]
fn go_crane_against_rust_registry() {
    let Some(bin) = go_crane() else { return };
    let go = Go { bin, home: tmp() };
    let disk = tmp();
    let r = Registry::start(disk.path());
    let a = r.addr.clone();
    let work = go.home.path();
    let layer = work.join("layer.tar");
    layer_tar(
        &layer,
        &[
            ("hello.txt", b"hello from crane-rs\n"),
            ("etc/os", b"test\n"),
        ],
    );
    let layer2 = work.join("layer2.tar");
    layer_tar(&layer2, &[("second.txt", b"two\n")]);

    // append: builds an image offline from the tarball and pushes it.
    let img = format!("{a}/test/img:v1");
    let pushed = go.ok(&["append", "-f", layer.to_str().unwrap(), "-t", &img]);
    let d = go.ok(&["digest", &img]).trim().to_string();
    assert!(digest::is_valid(&d), "{d}");
    assert_eq!(pushed.trim(), format!("{a}/test/img@{d}"));

    // manifest bytes hash to the digest; config is the one the manifest names.
    let manifest = go.ok(&["manifest", &img]);
    assert_eq!(digest::of(manifest.as_bytes()), d);
    let mv: Value = serde_json::from_str(&manifest).unwrap();
    let config = go.ok(&["config", &img]);
    assert_eq!(
        digest::of(config.as_bytes()),
        mv["config"]["digest"].as_str().unwrap()
    );
    let cv: Value = serde_json::from_str(&config).unwrap();
    assert_eq!(cv["rootfs"]["diff_ids"].as_array().unwrap().len(), 1);
    // Same bytes over raw HTTP.
    assert_eq!(
        r.get(&format!("/v2/test/img/manifests/{d}")).body,
        manifest.as_bytes()
    );

    // tag, ls
    go.ok(&["tag", &img, "v2"]);
    assert_eq!(go.ok(&["ls", &format!("{a}/test/img")]), "v1\nv2\n");
    assert_eq!(go.ok(&["digest", &format!("{a}/test/img:v2")]).trim(), d);

    // copy to another repo: blobs are mounted, digest preserved.
    let copy_dst = format!("{a}/other/copy:c1");
    let out = go.run(&["copy", &img, &copy_dst]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(log.contains("mounted blob"), "{log}");
    assert_eq!(go.ok(&["digest", &copy_dst]).trim(), d);
    go.ok(&["validate", "--remote", &copy_dst]);

    // pull to a tarball, push it back elsewhere: same digest.
    let tarball = work.join("img.tar");
    go.ok(&["pull", &img, tarball.to_str().unwrap()]);
    let rt = format!("{a}/roundtrip/img:rt");
    go.ok(&["push", tarball.to_str().unwrap(), &rt]);
    assert_eq!(go.ok(&["digest", &rt]).trim(), d);
    go.ok(&["validate", "--remote", &rt]);

    // A second image on top of the first, then an index of both.
    let img2 = format!("{a}/test/img:v3");
    go.ok(&[
        "append",
        "-b",
        &img,
        "-f",
        layer2.to_str().unwrap(),
        "-t",
        &img2,
    ]);
    let d2 = go.ok(&["digest", &img2]).trim().to_string();
    let idx = format!("{a}/test/idx:multi");
    go.ok(&["index", "append", "-m", &img, "-m", &img2, "-t", &idx]);
    let index = go.ok(&["manifest", &idx]);
    let iv: Value = serde_json::from_str(&index).unwrap();
    let children: Vec<&str> = iv["manifests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["digest"].as_str().unwrap())
        .collect();
    assert_eq!(children, [d.as_str(), d2.as_str()]);
    assert!(index.contains(OCI_INDEX));
    go.ok(&["validate", "--remote", &idx]);

    // catalog lists every repo.
    let cat = go.ok(&["catalog", &a]);
    assert_eq!(cat, "other/copy\nroundtrip/img\ntest/idx\ntest/img\n");

    // delete by digest: the manifest and its tags are gone.
    go.ok(&["delete", &format!("{a}/roundtrip/img@{d}")]);
    assert!(!go.run(&["manifest", &rt]).status.success());
    assert_eq!(
        r.get("/v2/roundtrip/img/tags/list").json()["tags"],
        json!([])
    );
    // delete by tag: only the tag.
    go.ok(&["delete", &format!("{a}/test/img:v2")]);
    assert_eq!(go.ok(&["ls", &format!("{a}/test/img")]), "v1\nv3\n");
}
