---
id: cr-u03z
status: open
open: true
deps: []
links: []
created: 2026-11-09T17:25:26Z
type: task
priority: 3
assignee: memgrafter
parent: cr-df8g
tags: [crane, registry]
---
# crane registry serve without $HOME: fails unless --disk is given

Found 2026-11-09 while writing the container ticket (cr-kzcr); split out at the user's request (separate topic).

Code path: src/registry/mod.rs storage_root(disk, HOME): --disk wins; else $HOME/.local/share/crane/registry; with neither it fails: 'no --disk given and $HOME is not set'. $HOME is usually unset in a FROM scratch container and in some service managers.

go crane differs: with no --disk it keeps the registry in memory, so it always starts.

Options (agent recommendation first):
1. Keep the error (clear message, no silent data location); document that containers and services pass --disk. Cost: none.
2. Fall back to a temp dir (std::env::temp_dir()/crane-registry) and print the path on stderr. Cost: data in /tmp can vanish on reboot without the user noticing.
3. Fall back to ./crane-registry in the working directory. Cost: data lands wherever the process happened to start.

## Acceptance Criteria

The user picks an option; if 2 or 3, a unit test for storage_root with HOME unset and a note in AGENTS.md; if 1, a line in AGENTS.md and in cr-kzcr's steps.

## Notes

**2026-11-09T18:00:56Z**

Checked go crane v0.22.1 source (module cache: cmd/crane/cmd/serve.go, pkg/registry/{registry,manifest,blobs,blobs_disk}.go): default storage is memory. --disk DIR stores only blob bytes on disk (DIR/sha256/<hex>, written to a temp file and renamed, no fsync). Manifests and tags (map repo -> tag/digest -> manifest, manifest.go) and in-progress uploads (uploads map[string][]byte, blobs.go) are always in memory, even with --disk. So a go registry restart loses every manifest and tag; blobs left on disk can be fetched by digest but no image can be pulled by tag. Hidden deprecated --blobs-to-disk = --disk on a new os.MkdirTemp dir. crane-rs instead keeps blobs, manifests, tags and uploads on disk, so it survives restarts. For this ticket: option 2 (temp dir) is closest to go's throwaway behaviour; agent still recommends option 1 (clear error).
