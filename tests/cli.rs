//! End-to-end behaviour of the binary that unit tests cannot see: the real
//! environment (DOCKER_CONFIG) and stderr logging.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

use common::*;

/// Registry that requires Basic u:p on every request; answers tags/list for repo "x".
fn basic_auth_registry() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut first = String::new();
            if r.read_line(&mut first).unwrap_or(0) == 0 {
                continue;
            }
            let mut authed = false;
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                    break;
                }
                // base64("u:p") = dTpw
                authed |= h
                    .to_ascii_lowercase()
                    .starts_with("authorization: basic dtpw");
            }
            let path = first
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();
            let (status, body) = match (path.as_str(), authed) {
                ("/v2/", false) => ("401 Unauthorized", String::new()),
                ("/v2/", true) => ("200 OK", String::new()),
                ("/v2/x/tags/list", true) => {
                    ("200 OK", r#"{"name":"x","tags":["ok"]}"#.to_string())
                }
                _ => ("401 Unauthorized", String::new()),
            };
            let mut w = s;
            let _ = write!(
                w,
                "HTTP/1.1 {status}\r\nWWW-Authenticate: Basic realm=\"r\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    addr
}

#[test]
fn logins_come_from_docker_config() {
    let addr = basic_auth_registry();
    let cfg = tempfile::tempdir().unwrap();
    std::fs::write(
        cfg.path().join("config.json"),
        format!(r#"{{"auths":{{"{addr}":{{"auth":"dTpw"}}}}}}"#),
    )
    .unwrap();
    let repo = format!("{addr}/x");
    let o = run_with_config(&rust_crane(), &["ls", &repo], cfg.path());
    assert!(o.ok, "{}", o.stderr);
    assert_eq!(o.text(), "ok\n");
    // without the login the registry refuses
    assert!(!run(&rust_crane(), &["ls", &repo]).ok);
    // a broken config file is an error, not silently ignored
    std::fs::write(cfg.path().join("config.json"), b"{").unwrap();
    let o = run_with_config(&rust_crane(), &["ls", &repo], cfg.path());
    assert!(!o.ok && o.stderr.contains("config.json"), "{}", o.stderr);
}

#[test]
fn verbose_logs_requests_to_stderr_only() {
    let reg = rust_registry();
    let o = ok(&rust_crane(), &["-v", "catalog", &reg.addr]);
    assert_eq!(o.text(), "");
    assert!(
        o.stderr
            .contains(&format!("GET http://{}/v2/_catalog -> 200", reg.addr)),
        "{}",
        o.stderr
    );
    let quiet = ok(&rust_crane(), &["catalog", &reg.addr]);
    assert_eq!(quiet.stderr, "");
}

#[test]
fn version_cp_alias_and_annotate_ref() {
    let rs = rust_crane();
    assert_eq!(
        ok(&rs, &["version"]).text(),
        format!("{}\n", env!("CARGO_PKG_VERSION"))
    );
    let reg = rust_registry();
    let dir = tempfile::tempdir().unwrap();
    let tar = dir.path().join("i.tar");
    std::fs::write(
        &tar,
        crane::fixture::docker_tarball(&[("a:b", &crane::fixture::Image::small(4))]),
    )
    .unwrap();
    let src = format!("{}/a/i:1", reg.addr);
    ok(&rs, &["push", tar.to_str().unwrap(), &src]);
    ok(&rs, &["cp", &src, &format!("{}/a/c:1", reg.addr)]);
    let lay = dir.path().join("lay");
    ok(
        &rs,
        &[
            "pull",
            "--format",
            "oci",
            "--annotate-ref",
            &src,
            lay.to_str().unwrap(),
        ],
    );
    let idx = std::fs::read_to_string(lay.join("index.json")).unwrap();
    assert!(
        idx.contains(&format!(r#""org.opencontainers.image.ref.name":"{src}""#)),
        "{idx}"
    );
}

#[test]
fn registry_stops_cleanly_on_sigterm() {
    let mut reg = rust_registry();
    let disk = reg.dir.path().to_path_buf();
    assert!(reg.stop(), "registry did not exit 0 on SIGTERM");
    // the storage lock was released: a new server on the same folder starts
    let o = std::process::Command::new(rust_crane())
        .args(["registry", "serve", "--address", "127.0.0.1:0", "--disk"])
        .arg(&disk)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut again = o;
    let mut line = String::new();
    BufReader::new(again.stderr.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(line.starts_with("listening on http://"), "{line}");
    let _ = again.kill();
    let _ = again.wait();
}
