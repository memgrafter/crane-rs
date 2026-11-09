---
id: cr-xfw3
status: closed
open: false
deps: [cr-u59o, cr-lrst]
links: []
created: 2026-11-09T13:57:35Z
type: task
priority: 1
assignee: memgrafter
parent: cr-df8g
tags: [crane, quality]
---
# Quality gates: 100% unit coverage, full integration coverage, mutation, performance, binary size

User request 2026-11-09. Coverage: cargo-llvm-cov 0.9.1 (installed), 100% line coverage from unit tests. Integration: every command, flag and registry route exercised end to end. Mutation: cargo-mutants 27.1.0 (installed). Performance: push/pull/copy throughput vs go crane on fixed generated data, regression thresholds. Binary size: release binary checked against a committed budget. All runnable from committed scripts.

## Notes

**2026-11-09T14:59:23Z**

Done 2026-11-09 (all gates scripted; numbers from this Mac, aarch64-apple-darwin, Rust 1.98.1, go crane v0.22.1 built by scripts/setup-tools.sh).
- Unit coverage (scripts/coverage.sh, cargo-llvm-cov 0.9.1): 100.00% lines (4197/4197), 100% functions, 98.08% regions. Gate: 100% lines. Test helpers (testutil.rs, */tests.rs, tests/) excluded.
- Integration-only coverage (scripts/coverage.sh --all, report, no gate): 89.00% lines. Registry test servers are stopped with SIGTERM so their coverage is written (crane registry serve now exits 0 on SIGINT/SIGTERM). Gaps are error branches covered by unit tests.
- Integration matrix: every command, flag and registry route has an end-to-end test (tests/cross.rs, tests/registry.rs, tests/cli.rs).
- Mutation (cargo-mutants 27.1.0): client files 459 mutants, last 5 survivors killed by request-level tests and checked with a targeted rerun (4/4 caught; 1 equivalent excluded); registry (agent run) 449 mutants, 5 survivors fixed; changed-code run 34 mutants, 1 unobservable (fsync no-op) excluded. Exclusions with reasons: .cargo/mutants.toml. Timeouts (hangs) count as caught.
- Performance (scripts/perf.sh, median of 3, gate rust <= 1.5x go): all 13 cases 0.57x-1.06x. Fixes found by the gate: sha2 0.10 used software SHA-256 on aarch64 (0.11 detects SHA instructions at runtime): push 3.3x -> 0.8x; pull output fsynced (F_FULLFSYNC) and unbuffered -> buffered, renamed, not synced like go: pull 4.5x -> 0.85x; registry F_FULLFSYNC per blob -> fsync(2): 200-blob push 9.4x -> 0.96x.
- Binary size (scripts/check-size.sh): 1.71 MiB (1797376 bytes), budget 2 MiB. Homebrew go crane is 11.9 MB.
