#!/usr/bin/env bash
# Reproducible tool setup for crane-rs tests. Idempotent.
#
#   .tools/bin/crane   go-containerregistry crane v0.22.1, the oracle for cross tests,
#                      built from source with -trimpath (needs Go; module source is
#                      fetched by the Go module proxy or read from the local module cache)
#   cargo-llvm-cov     0.9.1   coverage
#   cargo-mutants      27.1.0  mutation tests
#
# The Rust toolchain is pinned by rust-toolchain.toml; Cargo.lock pins crates.
set -euo pipefail
cd "$(dirname "$0")/.."

GO_CRANE_VERSION=v0.22.1
LLVM_COV_VERSION=0.9.1
MUTANTS_VERSION=27.1.0

mkdir -p .tools/bin
if [[ "$(.tools/bin/crane version 2>/dev/null || true)" != "${GO_CRANE_VERSION#v}" && "$(.tools/bin/crane version 2>/dev/null || true)" != "$GO_CRANE_VERSION" ]]; then
  echo "==> building go crane $GO_CRANE_VERSION into .tools/bin"
  GOBIN="$PWD/.tools/bin" GOFLAGS="-mod=mod -trimpath" go install "github.com/google/go-containerregistry/cmd/crane@$GO_CRANE_VERSION"
fi
echo "go crane: $(.tools/bin/crane version) ($(shasum -a 256 .tools/bin/crane | cut -c1-16)…, $(go version | awk '{print $3}'))"

need_cargo_tool() { # name version
  local have
  have="$(cargo "${1#cargo-}" --version 2>/dev/null | awk '{print $2}' || true)"
  if [[ "$have" != "$2" ]]; then
    echo "==> installing $1 $2 (have: ${have:-none})"
    cargo install --locked "$1@$2"
  fi
  echo "$1: $2"
}
need_cargo_tool cargo-llvm-cov "$LLVM_COV_VERSION"
need_cargo_tool cargo-mutants "$MUTANTS_VERSION"
rustup component add llvm-tools >/dev/null 2>&1 || true
