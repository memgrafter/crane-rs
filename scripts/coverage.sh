#!/usr/bin/env bash
# Coverage (cargo-llvm-cov 0.9.1, see scripts/setup-tools.sh).
#   unit:        `cargo llvm-cov --lib`, gate: 100% of lines in production code
#   integration: integration + cross tests ALONE (report only, no gate): what the real
#                binary covers end to end. Spawned crane binaries are instrumented; test
#                registries are stopped with SIGTERM so they write their coverage data.
# Test helpers (src/testutil.rs, */tests.rs) are excluded from the measurement.
set -euo pipefail
cd "$(dirname "$0")/.."
IGNORE='(testutil|/tests)\.rs$|/tests/'
cargo llvm-cov clean --workspace >/dev/null 2>&1 || true
echo "== unit coverage (gate: 100% lines)"
cargo llvm-cov --locked --lib --ignore-filename-regex "$IGNORE" --fail-under-lines 100 --summary-only
if [[ "${1:-}" == "--all" ]]; then
  echo "== integration-only coverage (report)"
  cargo llvm-cov clean --workspace >/dev/null 2>&1 || true
  cargo llvm-cov --locked --test cross --test registry --test cli \
    --ignore-filename-regex "$IGNORE|fixture\.rs$" --summary-only
fi
