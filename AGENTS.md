# crane-rs

Rust rewrite of a subset of go-containerregistry's `crane` (v0.22.1): a registry client and,
with `crane registry serve`, a registry. Binary name `crane`. Plan: `PLAN.md`. Epic: `.tickets/cr-df8g.md`.
Types: `crane.ts` is canonical; `src/types.rs` follows it.

## Commands (same args, flags and stdout as go crane)

| Cmd | Notes |
|---|---|
| `push <path> <ref>` | docker-save tarball or OCI layout dir (`--index` pushes index.json). Prints `<repo>@<digest>` |
| `pull <ref>... <path>` | `--format tarball` (default; digest refs tagged `i-was-a-digest`) or `oci` (whole index unless `--platform`; `--annotate-ref`) |
| `tag <ref> <tag>`, `copy\|cp <src> <dst>`, `delete <ref>` | copy mounts within a registry, uploads across |
| `digest [--full-ref]`, `manifest`, `config`, `blob <repo@digest>` | raw bytes, no trailing newline for manifest/config. Index + no `--platform`: config uses linux/amd64 (go default) |
| `ls [--full-ref] [-O] <repo>`, `catalog [--full-ref] <registry>` | pagination followed |
| `registry serve [--address host:port] [--disk dir]` | default `127.0.0.1:$PORT` or 5050, disk `~/.local/share/crane/registry` |
| `version` | |
| global | `--insecure`, `--platform os/arch[/variant][:osversion]` (`all` = none), `-v` |

## Rules

- Raw bytes: manifests and configs are never re-serialized. The only built manifest is the
  docker-save tarball one (`types::docker_manifest_bytes`), byte-identical to go's.
- Scheme: https; https then http for `--insecure`, RFC 1918, localhost, loopback (as go).
- Auth: `auths` in `$DOCKER_CONFIG/config.json` (else `~/.docker/config.json`); Basic or Bearer token flow. No credential helpers.
- stdout = results only; errors `Error: ...` on stderr, exit 1.

## Modules

`cli` (clap, dispatch) · `commands` · `client` (ureq; ping, auth, manifests, blobs, uploads, paging) ·
`auth` · `transfer` (Source/Sink copy of manifest trees) · `local` (tarballs, OCI layouts) ·
`reference` (go `name` rules) · `digest` · `types` · `registry/` (server) · `fixture` (deterministic test images) · `testutil` (test-only fake HTTP server)

## Dev

```sh
scripts/setup-tools.sh   # go crane v0.22.1 oracle -> .tools/bin, cargo-llvm-cov 0.9.1, cargo-mutants 27.1.0
./test.sh                # unit tests
./test.sh --all          # fmt, clippy, all tests incl. cross (needs the oracle), size gate
scripts/coverage.sh      # unit line coverage gate 100% (--all adds integration report)
scripts/mutants.sh       # mutation tests
scripts/perf.sh          # perf vs go crane, ratio gate (release)
scripts/check-size.sh    # release binary size budget
```

Cross tests (`tests/cross.rs`): {go, rust} client x {go, rust} registry must give identical
transcripts; go `validate --remote` must accept what rust pushes. Inputs come from `crane::fixture`
(same bytes every run); loopback only. Known registry difference: go's `_catalog` is unordered,
so catalog output is compared as a set.
