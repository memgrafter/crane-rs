---
id: cr-pou7
status: open
open: true
deps: []
links: []
created: 2026-11-09T18:05:22Z
type: task
priority: 3
assignee: memgrafter
tags: [crane, registry, tests]
---
# Run the OCI distribution-spec conformance suite against crane registry serve

Planned in the epic design (cr-df8g, 'Try the OCI distribution-spec conformance suite'), never tried. The cross tests only compare against go crane and go's registry, which do not use every route (e.g. referrers, chunked upload edge cases).

The suite is a Go test binary from github.com/opencontainers/distribution-spec (conformance/), configured with env vars (OCI_ROOT_URL, OCI_NAMESPACE, OCI_TEST_PULL/PUSH/CONTENT_DISCOVERY/CONTENT_MANAGEMENT). Check its current README for the exact setup before running. Build it reproducibly like the go crane oracle (scripts/setup-tools.sh, pinned version, .tools/bin).

## Acceptance Criteria

- Suite pinned and built by scripts/setup-tools.sh.
- A script or ignored test runs it against a fresh crane registry serve.
- Failures either fixed or listed on this ticket with a reason.
