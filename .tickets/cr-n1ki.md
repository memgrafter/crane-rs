---
id: cr-n1ki
status: open
open: true
deps: []
links: []
created: 2026-11-09T18:05:22Z
type: feature
priority: 3
assignee: memgrafter
tags: [crane, registry, durability]
---
# Registry durability: full drive flush once per manifest PUT (hybrid), idea

User asked (2026-11-09) whether a hybrid durability option is possible, rsync/rclone style (sync at the end, not per file). Answer: yes. Not started.

Today (src/registry/store.rs durable(), used by write_atomic and commit_upload): fsync(2) per blob on macOS. Survives app and OS crashes; a power cut can lose the newest blobs, because fsync(2) on macOS does not flush the drive's own cache.

Hybrid: keep fsync(2) per blob (moves data to the drive), and do one F_FULLFSYNC (drive-cache flush) when a manifest PUT commits, before answering 201. The manifest PUT is the point where an image becomes visible (its blobs must already exist), so after a 201 the whole image survives a power cut. Cost: one full flush per pushed manifest (about 6 ms here: 1.18 s / 200 measured for per-blob full flush) instead of one per blob. Other platforms: sync_all is already a full flush.

Measured on this Mac (200 small blobs): full flush per blob 1.18 s, fsync(2) 0.14 s, none 0.13 s.

## Acceptance Criteria

- Manifest PUT does one full flush before 201 (macOS); blob writes keep fsync(2).
- perf gate still passes (server push cases).
- Unit test covers the flush call path (100% unit coverage kept).
