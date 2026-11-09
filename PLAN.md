# crane-rs plan (draft, 2026-11-09)

A Rust rewrite of a small part of go-containerregistry's `crane` (Homebrew 0.22.1 on this Mac):
one binary that talks to OCI / Docker v2 registries and, with `crane registry serve`, is one.
Types first: `crane.ts` is the canonical type file; Rust follows it.

`crane serve` (no `registry`) is reserved for a later HTTP API over crane's own commands;
which commands deserve it is the deferred discovery ticket cr-ht88. Epic: cr-df8g.

Status (2026-11-09): v1 built. Client commands and `crane registry serve` are done; the
four-way cross test against go crane v0.22.1 passes; quality gates pass (100% unit line
coverage, mutation, perf within 1.5x of go, 2 MiB size budget). Commands, rules and dev
scripts: `AGENTS.md`. Tickets: epic cr-df8g (closed); serve without $HOME cr-u03z (P3);
fewer dependencies cr-888t (P1, planned, not started); registry gaps cr-t146 (P2: abandoned
uploads, garbage collection, accept backoff); `crane serve` API discovery cr-ht88 (deferred,
ready to start); container image cr-kzcr (deferred until a cluster use).

## Why a subset

The only known caller is a private release script:

```sh
crane push "$IMAGE_TAR" "$IMAGE" --insecure      # docker-save tarball
crane config "$IMAGE" --insecure | jq -e '.os == "linux" and .architecture == "amd64"'
crane tag "$IMAGE" latest --insecure
```

plus `crane manifest|config|ls` for inspection and
`crane manifest gcr.io/distroless/base-debian13:nonroot` to refresh a pinned base image. Go crane has 25 commands; most (mutate, rebase, flatten,
append, export, index, auth login) have no caller.

## v1 commands

| Cmd | Behavior | stdout |
|---|---|---|
| `push <path> <ref>` | docker-save tarball or OCI layout dir; HEAD each blob, upload missing, PUT manifest | `<registry>/<repo>@<digest>` |
| `pull <ref>... <path>` | `--format tarball` (default) or `oci` | nothing |
| `tag <ref> <tag>` | GET manifest bytes, PUT them under the new tag | nothing |
| `copy <src> <dst>` | blobs (cross-repo mount when same registry), then manifests; digest kept | nothing |
| `digest <ref>` | HEAD manifest, `Docker-Content-Digest` (GET + sha256 fallback) | `sha256:...` |
| `manifest <ref>` | raw bytes | bytes |
| `config <ref>` | raw bytes; index resolved via `--platform` or host | bytes |
| `blob <ref@digest>` | raw bytes, digest verified | bytes |
| `ls <repo>` | tags, follows `Link` pagination | one per line |
| `catalog <registry>` | repositories | one per line |
| `delete <ref>` | DELETE manifest | nothing |
| `registry serve` | OCI registry on a directory, see below | nothing (`listening on http://<addr>` on stderr) |

Global flags: `--insecure`, `--platform os/arch[/variant]`, `-v`. Same argument order and flag
names as go crane for every command above, so the release script runs unchanged.

Later, only on demand: credential helpers (`credsStore`), `auth login`, `validate`, `index`, `export`.

## `crane registry serve`

Why it is in v1 (user, 2026-11-09): a local container registry is wanted often, and it lets
crane test itself (client against its own registry) with no network or cluster.

- OCI Distribution Spec v1.1 routes in `crane.ts` (`Route`): blobs (monolithic + chunked
  upload, cross-repo mount), manifests (by tag or digest, Accept / Content-Type kept per manifest),
  tags/list and `_catalog` with `n` / `last` pagination, error bodies per `ErrorCode`.
- Flags as go crane: `--address host:port`, `--disk DIR`, `$PORT`. Differences from go crane:
  default address `127.0.0.1:5050` (go: `:0`, every interface), so LAN use needs an explicit
  `--address 0.0.0.0:5050`; storage is always on disk (go: in memory unless `--disk`).
- Storage: plain files under `--disk` (default `~/.local/share/crane/registry`), layout in `crane.ts`
  (`StorageLayout`). Temp file + fsync + rename for every write. `serve.lock` = one server per dir
  (tk queue pattern). Blob bytes are hashed while streaming to disk; never held in memory.
- Port 0 picks a free port and prints it (tests).
- No auth, no garbage collection in v1. `delete` removes the tag or manifest link only.

## Architecture (single crate, edition 2024)

```
crane.ts             canonical types (this draft)
generate-types.sh    crane.ts -> JSON Schema -> checks src/types.rs (uses bunx, not npx)
src/
  main.rs            clap derive, cmd_* dispatch
  types.rs           serde structs from crane.ts
  reference.rs       parse/normalize refs, digests
  client.rs          registry client: ping, auth challenge -> token, blobs, manifests, pagination
  auth.rs            docker config.json `auths`, bearer token cache
  local.rs           read/write docker-save tarballs and OCI layout dirs
  registry/
    mod.rs           routes, request handling
    store.rs         on-disk layout, locking, atomic writes
  http.rs            server-side HTTP/1.1 on std::net (keep-alive, Content-Length + chunked)
tests/
  oracle.rs          go crane as oracle (see Tests)
  registry.rs        start `crane registry serve --address 127.0.0.1:0`, drive it with both crane binaries
setup/install.sh     release build, atomic install
```

Crates: `clap`, `serde`, `serde_json`, `anyhow`, `thiserror`, `sha2`, `hex`,
`reqwest` (blocking, rustls; client only, as replmux), `tar`, `flate2` (only to compute
diff_ids when needed; push does not recompress). No async runtime. Dev: `tempfile`.

## Types pipeline

ticket-rs's `generate-types.sh` validates that schemas *generate* but then writes a fixed
heredoc, so `types.rs` can drift from `ticket.ts` without failing. For crane-rs:

1. `generate-types.sh` emits `schema/*.json` from `crane.ts` (typescript-json-schema via bunx)
   and commits them.
2. A Rust test serializes fixtures through `types.rs` and validates them against the
   committed schemas, and parses real manifests/configs captured from registries.
3. `types.rs` stays hand-written (serde renames like PascalCase `config.Env`,
   `os.version` need it) but drift fails the test.

## Tests

- Self-test: rust client against rust registry. Fast and offline, but it can hide a bug that
  both sides share (the same misreading of the spec), so it never runs alone; the checks below
  cross it with go crane.
- Oracle: go crane (`/opt/homebrew/bin/crane`) is installed. For each read command, compare
  rust vs go output byte-for-byte against both registries (rust and `crane registry serve` from go)
  and a public registry (gcr.io distroless, read-only).
- Cross matrix on port 0: {go, rust} client x {go, rust} registry; push one side, pull the other,
  digests equal.
- Real artifact: push a real docker-save tarball ($CRANE_REAL_TAR) to the rust registry, then
  `crane config | jq` exactly as the release script does.
- Conformance: the OCI distribution-spec conformance suite against the rust registry (to check it
  runs on this Mac; not yet tried).

## Build order

1. `reference.rs` + `types.rs` + schema test (no network).
2. `registry serve` store + routes, tested with go crane as the client. It comes first because it
   gives every later client test a local registry to run against.
3. Client read path: `digest`, `manifest`, `config`, `blob`, `ls`, `catalog`, anonymous token flow.
4. Write path: `push` (tarball, then OCI layout), `tag`, `copy`, `delete`.
5. `pull`.
6. Run the release script's three crane lines against a local `crane registry serve` with the rust binary.

## Decisions

- **D1 Binary name (final, 2026-11-09):** `crane`. The user handles the brew crane clash via PATH.
- **D2 Registry (final, 2026-11-09):** v1 includes the OCI registry as `crane registry serve`
  (go crane's name). `crane serve` is a later HTTP API over crane's commands: deferred, cr-ht88.
- **D3 Bind (final):** loopback by default; LAN only with an explicit `--address` (go's flag name,
  replacing the `--host` flag proposed earlier).
- **D4 Layout (final):** one crate at the repo root.
- **Agent's choices, not yet confirmed:** default port 5050 (not 5000, which macOS AirPlay
  can take); storage always on disk, default `~/.local/share/crane/registry`.
