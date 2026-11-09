//! Performance tests (cr-xfw3). Ignored by default; run with `scripts/perf.sh`
//! (release build). Compares crane-rs against the pinned go crane on the same
//! machine, data and registry, so the check is a ratio, not wall-clock seconds.
//!
//! Each case: fresh registry, median of PERF_RUNS (default 3) runs.
//! Fails if rust_median > go_median * PERF_MAX_RATIO (default 1.5).
//! Results: .bench/perf-<unix time>.json

mod common;

use std::path::{Path, PathBuf};
use std::time::Instant;

use common::*;
use crane::fixture::{Image, docker_tarball};

fn env_f64(k: &str, d: f64) -> f64 {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn time(bin: &Path, args: &[&str]) -> f64 {
    let t = Instant::now();
    ok(bin, args);
    t.elapsed().as_secs_f64()
}

struct Case {
    name: &'static str,
    /// (registry kind for the run, fixture tarball)
    reg_kind: &'static str,
    tar: PathBuf,
    op: &'static str,
}

fn registry(kind: &str) -> Reg {
    if kind == "go" {
        go_registry()
    } else {
        rust_registry()
    }
}

/// One timed run of `op` with `client` on a fresh registry; setup steps are untimed.
fn run_case(client: &Path, setup_client: &Path, c: &Case) -> f64 {
    let reg = registry(c.reg_kind);
    let img = format!("{}/perf/img:1", reg.addr);
    let tar = c.tar.display().to_string();
    let work = tempfile::tempdir().unwrap();
    match c.op {
        "push" => time(client, &["push", &tar, &img]),
        "pull" => {
            ok(setup_client, &["push", &tar, &img]);
            time(
                client,
                &[
                    "pull",
                    &img,
                    &work.path().join("o.tar").display().to_string(),
                ],
            )
        }
        "copy-cross-registry" => {
            ok(setup_client, &["push", &tar, &img]);
            let other = registry(c.reg_kind);
            time(
                client,
                &["copy", &img, &format!("{}/perf/copy:1", other.addr)],
            )
        }
        "digest-x50" => {
            ok(setup_client, &["push", &tar, &img]);
            let t = Instant::now();
            for _ in 0..50 {
                ok(client, &["digest", &img]);
            }
            t.elapsed().as_secs_f64()
        }
        other => panic!("unknown op {other}"),
    }
}

#[test]
#[ignore = "performance: run scripts/perf.sh"]
fn rust_is_not_slower_than_go() {
    let (go, rs) = (go_crane(), rust_crane());
    let runs = env_f64("PERF_RUNS", 3.0) as usize;
    let max_ratio = env_f64("PERF_MAX_RATIO", 1.5);
    let big_mib = env_f64("PERF_BIG_MIB", 256.0) as usize;

    let data = tempfile::tempdir().unwrap();
    let big = data.path().join("big.tar");
    std::fs::write(
        &big,
        docker_tarball(&[(
            "perf/big:1",
            &Image::new(1, 1, big_mib << 20, "linux", "amd64"),
        )]),
    )
    .unwrap();
    let many = data.path().join("many.tar");
    std::fs::write(
        &many,
        docker_tarball(&[(
            "perf/many:1",
            &Image::new(2, 200, 64 << 10, "linux", "amd64"),
        )]),
    )
    .unwrap();

    let mut cases = Vec::new();
    for reg_kind in ["go", "rust"] {
        for (op, tar) in [
            ("push", &big),
            ("push", &many),
            ("pull", &big),
            ("copy-cross-registry", &big),
            ("digest-x50", &many),
        ] {
            let name: &'static str = Box::leak(
                format!(
                    "{op} {} -> {reg_kind} registry",
                    tar.file_name().unwrap().to_string_lossy()
                )
                .into_boxed_str(),
            );
            cases.push(Case {
                name,
                reg_kind,
                tar: tar.clone(),
                op,
            });
        }
    }

    let mut rows = Vec::new();
    let mut failures = Vec::new();
    for c in &cases {
        let g = median((0..runs).map(|_| run_case(&go, &go, c)).collect());
        let r = median((0..runs).map(|_| run_case(&rs, &go, c)).collect());
        let ratio = r / g;
        eprintln!(
            "{:<45} go {g:>8.3}s  rust {r:>8.3}s  ratio {ratio:>5.2}",
            c.name
        );
        rows.push(serde_json::json!({"case": c.name, "go_s": g, "rust_s": r, "ratio": ratio}));
        if ratio > max_ratio {
            failures.push(format!(
                "{}: rust {r:.3}s vs go {g:.3}s (ratio {ratio:.2} > {max_ratio})",
                c.name
            ));
        }
    }
    // Server side: go client against each registry kind, same data.
    for (op, tar) in [("push", &big), ("pull", &big), ("push", &many)] {
        let mk = |k: &'static str| Case {
            name: "server",
            reg_kind: k,
            tar: tar.clone(),
            op,
        };
        let g = median((0..runs).map(|_| run_case(&go, &go, &mk("go"))).collect());
        let r = median((0..runs).map(|_| run_case(&go, &go, &mk("rust"))).collect());
        let ratio = r / g;
        let name = format!(
            "server: go client {op} {}, rust vs go registry",
            tar.file_name().unwrap().to_string_lossy()
        );
        eprintln!("{name:<45} go {g:>8.3}s  rust {r:>8.3}s  ratio {ratio:>5.2}");
        rows.push(serde_json::json!({"case": name, "go_s": g, "rust_s": r, "ratio": ratio}));
        if ratio > max_ratio {
            failures.push(format!("{name}: ratio {ratio:.2} > {max_ratio}"));
        }
    }

    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join(".bench");
    std::fs::create_dir_all(&out).unwrap();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let report = serde_json::json!({
        "runs": runs, "max_ratio": max_ratio, "big_mib": big_mib,
        "go_crane": GO_CRANE_VERSION, "results": rows,
    });
    std::fs::write(
        out.join(format!("perf-{stamp}.json")),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    assert!(
        failures.is_empty(),
        "slower than go:\n{}",
        failures.join("\n")
    );
}
