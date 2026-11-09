---
id: cr-df8g
status: closed
open: false
deps: []
links: []
created: 2026-11-09T13:34:38Z
type: epic
priority: 1
assignee: memgrafter
tags: [crane, rust]
---
# crane-rs: Rust rewrite of go-containerregistry crane (v1 subset + crane registry serve)

Rewrite a small part of go-containerregistry's `crane` (Homebrew 0.22.1 at /opt/homebrew/bin/crane) as one Rust binary. It talks to OCI / Docker v2 registries and can act as one with `crane registry serve`. `crane serve` (an HTTP API over crane's own commands) is deferred: cr-ht88. The user wants a dead simple interface. Patterns to follow: ticket-rs and replmux (replmux is the newer one). Types come first: `crane.ts` is the canonical type file and the Rust types follow it.

Full plan: PLAN.md. Draft types: crane.ts (committed df4b250; passes `bunx -p typescript tsc --noEmit --strict`).

**Why only part of crane**
Only known caller: a private release script
    crane push "$IMAGE_TAR" "$IMAGE" --insecure   (docker-save tarball written by go-containerregistry tarball.WriteToFile)
    crane config "$IMAGE" --insecure | jq -e '.os == "linux" and .architecture == "amd64"'
    crane tag "$IMAGE" latest --insecure
Also `crane manifest|config|ls` for inspection, and `crane manifest gcr.io/distroless/base-debian13:nonroot` to refresh a pinned base image. Target registry: a plain-HTTP registry:2 on the LAN.

**v1 commands (same argument order and flags as go crane)**
push <path> <ref>, pull <ref>... <path>, tag <ref> <tag>, copy <src> <dst>, digest, manifest, config, blob, ls <repo>, catalog <registry>, delete <ref>, registry serve.
Global flags: --insecure, --platform os/arch[/variant], -v.
stdout carries only the result (raw bytes or one value per line); progress goes to stderr.
Not in v1: mutate, rebase, flatten, append, export, index, auth login, credential helpers (~/.docker/config.json has no stored logins today).

**Rules**
- Raw bytes: a digest is sha256 over the exact bytes. The parsed types describe what crane reads; crane never re-serializes a manifest or config to push, store or print it.
- Logins: `auths` in docker config.json, then anonymous; bearer token flow on a 401, tokens cached per (registry, scope).

## Design

**Decisions**
- D1 (final, 2026-11-09): the binary is named `crane`. The user handles the clash with the brew crane through PATH.
- D2 (final, 2026-11-09): v1 includes the OCI registry, named `crane registry serve` like go crane. The user's reasons: it is a useful component, crane can test itself against it, and the user wants a local container registry very often. `crane serve` is kept for a later HTTP API over crane's commands; discovery ticket cr-ht88 (P1, deferred).
- D3 (final): the registry binds 127.0.0.1 by default; LAN use needs an explicit `--address` (go crane's flag name, replacing the `--host` proposed earlier).
- D4 (final): one crate at the repo root (ticket-rs layout), not a cli/ subfolder.

**Assumptions the agent made (reversible, not yet confirmed by the user)**
- Default registry address 127.0.0.1:5050 ($PORT overrides the port), not port 5000 (macOS AirPlay Receiver can take 5000; 5000 was free on this Mac on 2026-11-09).
- Storage always on disk, default ~/.local/share/crane/registry (go crane keeps it in memory unless --disk); tests pass a temp dir.

**Architecture (edition 2024, no async runtime)**
crane.ts; generate-types.sh (uses bunx, because npx is not installed on this Mac); src/{main.rs (clap derive + cmd_*), types.rs, reference.rs, client.rs, auth.rs, local.rs (docker-save tarball + OCI layout), registry/{mod.rs, store.rs}, http.rs (std::net server, keep-alive, Content-Length + chunked, streamed bodies)}; tests/{oracle.rs, registry.rs}; setup/install.sh.
Crates: clap, serde, serde_json, anyhow, thiserror, sha2, hex, reqwest (blocking, rustls, client only, as in replmux), tar, flate2. Dev: tempfile.

**crane registry serve**
Flags as go crane: --address host:port, --disk DIR, $PORT. OCI Distribution Spec v1.1 routes (crane.ts `Route`): blobs (one-request and chunked uploads, cross-repo mount), manifests by tag or digest (stored Content-Type kept), tags/list and _catalog with n/last paging, `ErrorCode` error bodies. Plain-file storage (crane.ts `StorageLayout`): write to a temp file, fsync, rename. serve.lock allows one server per folder. Blobs are hashed while they stream to disk. Port 0 picks a free port and prints it. v1 has no auth and no garbage collection.

**Types pipeline**
ticket-rs's generate-types.sh checks that schemas generate, then writes a fixed block of Rust, so types.rs can drift from ticket.ts unnoticed. Here: commit schema/*.json made from crane.ts; types.rs stays hand-written (serde renames), and a Rust test checks fixtures and real manifests against the schemas, so drift fails the test.

**Tests**
Self-test (rust client against rust registry) is fast and offline but can hide a bug both sides share, so it is always crossed with go crane: the {go, rust} client x {go, rust} registry matrix, pushing on one side and pulling on the other, digests equal. Read commands compared byte for byte with go crane (local registries plus read-only gcr.io distroless). Real artifact: a real docker-save tarball ($CRANE_REAL_TAR). Try the OCI distribution-spec conformance suite (not yet tried).

**Build order**
1 reference.rs + types.rs + schema test. 2 registry serve store + routes, with go crane as the client. 3 read commands + anonymous token flow. 4 push (tarball, then OCI layout), tag, copy, delete. 5 pull. 6 the release script's three crane lines against a local `crane registry serve`, using the Rust binary.

## Acceptance Criteria

- `crane registry serve` passes the cross matrix with go crane.
- The three crane lines from the release script run unchanged with the Rust `crane`, against a local registry, on a real app tarball.
- Every v1 read command's output matches go crane byte for byte on the same refs.
- Schema test: types.rs matches crane.ts.

## Notes

**2026-11-09T13:34:48Z**

D2 answer given to the user (2026-11-09): go crane is not an HTTP API. It is a CLI client of the registry HTTP API (OCI Distribution, /v2/...). The registry, e.g. registry:2 in the cluster, is the HTTP service in front of the stored images. Go crane does ship a registry: `crane registry serve` (--address, --disk; checked with --help on 0.22.1). Value of a Rust `crane serve` here: mainly offline tests and a local place to try a push before touching the cluster. It would not replace the cluster's registry:2. Agent's recommendation: leave serve out of v1, use go `crane registry serve --disk` as the test registry, and add `crane serve` only when a real use appears. Waiting on the user.

**2026-11-09T13:40:00Z**

User decided D2: keep the registry in v1 as `crane registry serve`, and make `crane serve` a deferred P1 discovery ticket (cr-ht88) to find which crane commands are good abstractions to serve through their own API. Updated crane.ts and PLAN.md to match.

**2026-11-09T17:12:25Z**

Status 2026-11-09: v1 built and verified; epic stays OPEN for one unmet acceptance criterion (schema test, below). Verified today with ./test.sh --all (fmt, clippy, 135 unit + 4 cli + 4 cross + 11 registry tests pass; size gate 1797376 bytes, budget 2097152). Commits b8b6d69..464396d on main (local only, not pushed).

**Acceptance criteria**
- MET: `crane registry serve` passes the cross matrix with go crane v0.22.1 (tests/cross.rs four_way_transcripts_match, go_validates_what_rust_pushes_to_rust_registry, copy_between_registries_both_directions).
- MET: the release script's three lines (push --insecure, config os/arch check, tag latest) run with the Rust crane on a real app tarball ($CRANE_REAL_TAR), against both registries, from both clients; same digest in all four pairings (real_tarball_when_given; checks config with serde_json instead of jq).
- MET on local refs: every v1 read command's output matches go crane byte for byte in the four-way transcript. NOT done: the read-only comparison against gcr.io distroless from the Design section; dropped so the tests stay loopback-only and reproducible.
- NOT MET: schema test (types.rs checked against crane.ts). No schema/*.json and no drift test exist. crane.ts and types.rs were synced by hand (commit 12a28b2).

**Built (child tickets, all closed)**
- cr-lrst: client commands push, pull (tarball / oci, --annotate-ref), tag, copy|cp, digest (--full-ref), manifest, config, blob, ls (--full-ref, -O), catalog (--full-ref), delete, version; global --insecure, --platform, -v.
- cr-u59o: `crane registry serve` (src/registry/: http.rs, store.rs, routes.rs, mod.rs, signals.rs); OCI Distribution v1.1 routes incl. chunked uploads, mount, referrers; stops cleanly on SIGINT/SIGTERM.
- cr-uzkn: reproducible cross tests (go oracle built by scripts/setup-tools.sh; inputs from crane::fixture; loopback only).
- cr-xfw3: quality gates: unit line coverage 100% (4197/4197, gated); integration-only coverage 89% (reported, not gated); mutation clean apart from 3 documented exclusions in .cargo/mutants.toml; perf 0.57x-1.06x of go (gate 1.5x); size 1.71 MiB (gate 2 MiB; Homebrew go crane 11.9 MB).

**Built differently from the Design section**
- HTTP client ureq 3 (rustls), not reqwest. No thiserror, hex or flate2: anyhow for errors, own hex/base64, no gzip (push accepts only gzip layers as stored; pull writes them as-is).
- Deps: anyhow, clap, serde, serde_json, sha2 0.11, tar, ureq, libc (unix). Dev: tempfile.
- Layout: lib + bin in one crate; src/{cli, commands, client, auth, transfer, local, reference, digest, types, fixture, testutil}.rs and src/registry/; tests/{cross, registry, cli, perf}.rs; scripts/{setup-tools, coverage, mutants, perf, check-size}.sh; test.sh. No generate-types.sh, setup/install.sh or tests/oracle.rs.
- Registry storage per repo uses _blobs/_manifests/_tags (crane.ts StorageLayout updated).
- Registry durability: fsync(2) per blob on macOS instead of F_FULLFSYNC (survives process/OS crash; a power cut can lose the newest blobs). go's registry never syncs. pull output is not synced (as go).
- OCI conformance suite: not tried.

**Known registry gaps** (also on cr-u59o): abandoned uploads never cleaned up; no garbage collection; accept() errors retry without backoff; any repo can finish another repo's upload id (as go).

**Still open elsewhere**
- cr-888t vendoring plan: written, NOT started. Questions for the user: goal offline builds or fewer deps? ~35 MB vendored source in git OK?
- cr-ht88 `crane serve` API discovery: deferred, P1.
- cr-kzcr crane container image: deferred, P3; recommendation: wait for a concrete cluster use.

**Agent choices awaiting user acknowledgement (reversible)**
Storage always on disk (~/.local/share/crane/registry); default address 127.0.0.1:5050; tickets committed to git; ureq; push rejects uncompressed layers; perf gate 1.5x; size budget 2 MiB; _blobs/_manifests/_tags names; TAG_INVALID not in crane.ts ErrorCode (not an OCI v1.1 code); integration coverage reported not gated; pull output not synced; registry fsync(2) not F_FULLFSYNC; registry stops cleanly on SIGINT/SIGTERM.

**To close this epic**: add the schema test, or the user drops that criterion.

**2026-11-09T17:15:09Z**

User answers 2026-11-09: dependencies goal is fewer dependencies, P1 (cr-888t rewritten around replacing crates; vendor/ dropped, so the 35 MB question no longer applies). Registry gaps ticketed as cr-t146 (P2, agent's pick). cr-ht88 given a ready-to-start brief, still deferred. cr-kzcr given a start trigger, steps and acceptance criteria, still deferred. Epic still open only for the schema test.

**2026-11-09T17:27:48Z**

2026-11-09: last acceptance criterion met. User chose option B for the schema test: src/types/tests.rs (module types::crane_ts) reads field names of 14 Rust serde types from serde itself and compares them with crane.ts interfaces (exact for full types, subset for parsed views). It found 3 drifts on its first run, fixed in crane.ts: Platform.features, ImageConfig 'os.version', TarballManifestEntry.LayerSources (commit 8ba2037). ./test.sh --all and 100% unit coverage pass. All four acceptance criteria met (read-command byte match on local registries; gcr.io comparison dropped to keep tests offline). User acknowledged all 16 listed choices. Epic closed; follow-ups stay open on their own: cr-888t (fewer deps, P1), cr-t146 (registry gaps, P2), cr-u03z (serve without HOME, P3), cr-ht88 and cr-kzcr (deferred).
