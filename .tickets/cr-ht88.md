---
id: cr-ht88
status: deferred
open: true
deps: []
links: []
created: 2026-11-09T13:37:34Z
type: task
priority: 1
assignee: memgrafter
parent: cr-df8g
tags: [crane, serve, discovery]
---
# crane serve: find which crane commands deserve their own HTTP API (discovery)

Deferred (2026-11-09, user): `crane serve` will be crane's own HTTP API: crane commands offered as an API, the way replmux's service offers runtimes. It is a different thing from `crane registry serve`, the OCI registry built in v1 under parent cr-df8g. This ticket is discovery only: work out which crane commands are good abstractions to serve through their own API. Build nothing until the user approves the result.

Starting points (the agent's guesses, not decided):
- A command earns an API route if it does at least one of these: combines several registry calls into one meaningful step (e.g. "promote": copy + tag + check the platform, which the release script does by hand today); holds state worth keeping between calls (login tokens, open connections, a cache of manifests by digest); or runs long and needs progress or cancelling (push or copy of large images).
- A command does NOT earn a route if it is a single registry call: manifest, blob, ls and catalog are already HTTP through the registry's own /v2 API. Wrapping them adds nothing.
- Possible clients: agents (pi extension, MCP like `replmux mcp`), scripts in other languages, and the k3s release flow.
- Open design points: same process and port as `crane registry serve` (e.g. /v1/... next to /v2/...) or a separate process; loopback-only like tk queue serve and the registry (D3).

## Acceptance Criteria

- Every v1 crane command, plus any combined step found (e.g. promote), sorted into served or not served, with one reason each.
- For the served ones: request and response interfaces drafted in crane.ts, checked with tsc.
- A recommendation on shared versus separate process, and on who the clients are.
- The user approves before any Rust is written.

## Notes

**2026-11-09T17:14:58Z**

**Ready-to-start brief (2026-11-09; user asked that this ticket be fully ready; status stays deferred until the user says start)**

Inputs to read (paths checked 2026-11-09)
- Callers today: a private release script (push, config | jq os/arch check, tag latest); manual config, manifest, ls for inspection; manifest of gcr.io/distroless/base-debian13:nonroot to refresh a pinned base image.
- crane-rs surface: AGENTS.md command table; crane.ts `Command` union and `Route` union; src/commands.rs (what each command does in registry calls).
- Service patterns to compare: replmux `serve` (broker on a Unix socket, typed KernelRequest/KernelResponse, replmux/cli/src/broker.rs) and `replmux mcp` (stdio MCP, replmux/cli/src/mcp.rs); ticket-rs `tk queue serve` (127.0.0.1 only, append-only log, ticket-rs/src/queue.rs).

Method
1. Table of every v1 command (push, pull, tag, copy, digest, manifest, config, blob, ls, catalog, delete, registry serve, version) plus combined steps seen in callers (candidate: promote = push or copy, check config os/arch, tag; candidate: mirror = copy from a public registry into the cluster registry). Columns: registry calls it makes; state that would be worth keeping between calls (tokens, manifest cache by digest); long-running or needs progress/cancel; who would call it; verdict served / not served; one-line reason. Rules for the verdict are the "Starting points" above.
2. For each served item: request and response types in crane.ts (new `ServeRequest` / `ServeResponse` unions, errors reusing the `Error: ...` text), checked with `bunx -p typescript tsc --noEmit --strict crane.ts`.
3. Transport recommendation with tradeoffs: (a) HTTP /v1/... on the same process and port as `crane registry serve`; (b) separate `crane serve` on its own loopback port; (c) Unix socket like replmux; (d) MCP over stdio for agents. Include which clients each one serves.
4. Write the result as a note on this ticket (table + recommendation + open questions), then ask the user. Build no Rust before approval.

Constraints: loopback only by default (decision D3); stdout/stderr rules from AGENTS.md still apply to CLI commands; no new dependencies without checking cr-888t (fewer dependencies is the P1 goal); an agent doing this spawns no subagents of its own.

Done check: `bunx -p typescript tsc --noEmit --strict crane.ts` passes; the table covers all 13 commands; every served item has types; the user has approved or changed the recommendation (recorded here).
