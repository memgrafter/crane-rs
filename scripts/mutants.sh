#!/usr/bin/env bash
# Mutation tests (cargo-mutants 27.1.0). Fails if any mutant survives ("missed").
# Args pass through, e.g. scripts/mutants.sh --file src/reference.rs
set -euo pipefail
cd "$(dirname "$0")/.."
cargo mutants --timeout 120 -j "${MUTANTS_JOBS:-4}" "$@"
