#!/usr/bin/env bash
# ./test.sh          unit tests (quick)
# ./test.sh --all    fmt, clippy, unit + integration + cross tests, binary size
# Reproducible: toolchain from rust-toolchain.toml, crates from Cargo.lock (--locked),
# go crane oracle and cargo tools from scripts/setup-tools.sh.
set -euo pipefail
cd "$(dirname "$0")"
if [[ "${1:-}" != "--all" ]]; then
  exec cargo test --locked --lib
fi
./scripts/setup-tools.sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
./scripts/check-size.sh
