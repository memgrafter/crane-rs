#!/usr/bin/env bash
# Release binary size gate. Budget is per target; raise it only on purpose, in its own commit.
set -euo pipefail
cd "$(dirname "$0")/.."
target="$(rustc -vV | sed -n 's/^host: //p')"
case "$target" in  # (case, not an associative array: macOS /bin/bash is 3.2)
  aarch64-apple-darwin) budget=2097152 ;;  # 2 MiB; measured 1.71 MiB on 2026-11-09 (registry + client)
  *) budget= ;;
esac
cargo build --locked --release >/dev/null 2>&1
size=$(wc -c < target/release/crane | tr -d ' ')
printf 'crane (release, %s): %d bytes (%.2f MiB)\n' "$target" "$size" "$(echo "$size/1048576" | bc -l)"
if [[ -z "$budget" ]]; then
  echo "no size budget for $target; add one to scripts/check-size.sh" >&2
  exit 1
fi
if (( size > budget )); then
  echo "FAIL: over budget ($budget bytes) by $((size - budget)) bytes" >&2
  exit 1
fi
echo "ok: within budget ($budget bytes, $((budget - size)) to spare)"
