//! Shared harness for integration and cross tests: start registries (rust and go),
//! run either crane binary with a timeout and an empty docker config.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const GO_CRANE_VERSION: &str = "v0.22.1";

pub fn rust_crane() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_crane"))
}

/// The pinned go crane oracle: $CRANE_GO, else <repo>/.tools/bin/crane.
/// Missing or wrong version fails the test (run scripts/setup-tools.sh), so a cross
/// test can never pass by silently skipping.
pub fn go_crane() -> PathBuf {
    let p = std::env::var_os("CRANE_GO")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join(".tools/bin/crane"));
    let out = Command::new(&p)
        .arg("version")
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "go crane oracle missing at {} ({e}); run scripts/setup-tools.sh",
                p.display()
            )
        });
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(
        v,
        GO_CRANE_VERSION,
        "go crane at {} is {v}, want {GO_CRANE_VERSION}",
        p.display()
    );
    p
}

pub struct Reg {
    pub addr: String,
    pub kind: &'static str,
    child: Child,
    pub dir: tempfile::TempDir,
}

impl Reg {
    /// SIGTERM, then wait up to 5 s (SIGKILL after that). Returns whether it exited 0.
    /// A clean exit also lets an instrumented binary write its coverage data.
    pub fn stop(&mut self) -> bool {
        // SAFETY: plain kill(2) on our own child's pid.
        unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s.success();
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Reg {
    fn drop(&mut self) {
        self.stop();
    }
}

fn start(bin: &Path, kind: &'static str, parse: fn(&str) -> Option<String>) -> Reg {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(bin)
        .args(["registry", "serve", "--address", "127.0.0.1:0", "--disk"])
        .arg(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut sent = false;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if !sent && let Some(a) = parse(&line) {
                let _ = tx.send(a);
                sent = true;
            }
        }
    });
    let addr = rx
        .recv_timeout(Duration::from_secs(20))
        .unwrap_or_else(|_| panic!("{kind} registry did not start"));
    Reg {
        addr,
        kind,
        child,
        dir,
    }
}

pub fn rust_registry() -> Reg {
    start(&rust_crane(), "rust", |l| {
        l.split("listening on http://")
            .nth(1)
            .map(|s| s.trim().to_string())
    })
}

pub fn go_registry() -> Reg {
    start(&go_crane(), "go", |l| {
        l.split("serving on port ")
            .nth(1)
            .map(|p| format!("127.0.0.1:{}", p.trim()))
    })
}

#[derive(Debug, Clone)]
pub struct Out {
    pub ok: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Out {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// Run `bin args` with an empty DOCKER_CONFIG and a 120 s limit.
pub fn run(bin: &Path, args: &[&str]) -> Out {
    let cfg = tempfile::tempdir().unwrap();
    run_with_config(bin, args, cfg.path())
}

/// Run with DOCKER_CONFIG set to `docker_config`.
pub fn run_with_config(bin: &Path, args: &[&str], docker_config: &Path) -> Out {
    let mut child = Command::new(bin)
        .args(args)
        .env("DOCKER_CONFIG", docker_config)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut so = child.stdout.take().unwrap();
    let mut se = child.stderr.take().unwrap();
    let t1 = std::thread::spawn(move || {
        let mut v = Vec::new();
        so.read_to_end(&mut v).unwrap();
        v
    });
    let t2 = std::thread::spawn(move || {
        let mut s = String::new();
        se.read_to_string(&mut s).unwrap();
        s
    });
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("timeout: {} {args:?}", bin.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Out {
        ok: status.success(),
        stdout: t1.join().unwrap(),
        stderr: t2.join().unwrap(),
    }
}

/// Run and require success.
pub fn ok(bin: &Path, args: &[&str]) -> Out {
    let o = run(bin, args);
    assert!(o.ok, "{} {args:?} failed: {}", bin.display(), o.stderr);
    o
}
