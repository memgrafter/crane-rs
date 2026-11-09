#!/usr/bin/env bash
# Performance vs the pinned go crane (tests/perf.rs). Release build.
# Env: PERF_RUNS (3), PERF_MAX_RATIO (1.5), PERF_BIG_MIB (256). Results in .bench/.
set -euo pipefail
cd "$(dirname "$0")/.."
./scripts/setup-tools.sh >/dev/null
cargo test --locked --release --test perf -- --ignored --nocapture
