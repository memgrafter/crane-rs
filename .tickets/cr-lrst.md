---
id: cr-lrst
status: closed
open: false
deps: []
links: []
created: 2026-11-09T13:57:35Z
type: feature
priority: 1
assignee: memgrafter
parent: cr-df8g
tags: [crane, client]
---
# crane client commands: push pull tag copy digest manifest config blob ls catalog delete

Implement src/commands.rs + client/auth/local modules. Same args/flags/stdout as go crane v0.22.1.

## Notes

**2026-11-09T14:49:13Z**

Done 2026-11-09: all v1 client commands; output identical to go crane v0.22.1 in the cross tests.
