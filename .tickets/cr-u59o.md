---
id: cr-u59o
status: closed
open: false
deps: []
links: []
created: 2026-11-09T13:57:35Z
type: feature
priority: 1
assignee: memgrafter
parent: cr-df8g
tags: [crane, registry]
---
# crane registry serve: OCI Distribution registry on a directory

Implement src/registry/ behind the fixed stub interface (ServeConfig, Server::start/addr/shutdown, serve). Routes and storage layout: crane.ts (Route, StorageLayout, ErrorCode). Built by a subagent in a worktree; 100% unit line coverage of src/registry/** required (cr quality ticket).

## Notes

**2026-11-09T14:34:20Z**

Built (branch worktree-agent-a8f19afb82f8df6c8). src/registry/: http.rs (server HTTP/1.1 over std::net: keep-alive, Content-Length and chunked bodies streamed, Expect 100-continue, HEAD without body, first-byte rejection of non-HTTP such as a TLS ClientHello -> 400 + close, lingering close so the peer reads the error), store.rs (crane.ts StorageLayout; temp file + fsync + rename for every write; blobs hashed while streaming, running hash cached per upload and rebuilt from the file after a restart), routes.rs (every crane.ts Route plus upload status/cancel and the referrers API with artifactType filter and OCI-Filters-Applied; OCI-Subject on PUT), mod.rs (Server start/addr/shutdown, serve.lock via File::try_lock, $PORT and $HOME defaults, verbose log line per request; shutdown wakes the accept loop, closes idle keep-alive connections and joins every thread). Layout change: per-repo dirs are _blobs/_manifests/_tags (crane.ts StorageLayout updated) because repo 'a/blobs' would otherwise collide with repo 'a' markers. Error code TAG_INVALID is used (brief) but is not in crane.ts ErrorCode. Unit-test line coverage (cargo llvm-cov --lib --summary-only): http.rs 521/521, mod.rs 199/199, routes.rs 517/517, store.rs 526/526 = 100%. Integration: tests/registry.rs, 11 tests through the real binary incl. go crane v0.22.1 (append, digest, manifest, config, tag, ls, copy with mount, pull+push, index append, validate --remote, catalog, delete). Mutation (cargo mutants --file 'src/registry/**' --timeout 60 -j 4): 449 mutants, 354 caught, 24 timeouts (hangs in shutdown/keep-alive paths, counted as detected), 66 unviable, 5 missed; all 5 then fixed (3 killed by new tests, confirmed by a targeted rerun: 10/10 caught; 2 were always-true conditions, removed). Known gaps: no cleanup of abandoned uploads/; no garbage collection of unreferenced blobs; an accept() error (e.g. out of file descriptors) retries immediately without backoff; a shutdown during a request body upload fails that request; uploads are not bound to the repo that started them (any repo can finish an upload id, as in go's registry).
