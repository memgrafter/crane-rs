# crane-rs

A small Rust rewrite of [go-containerregistry](https://github.com/google/go-containerregistry)'s
`crane`: a container registry client, and with `crane registry serve`, a registry.
Same commands, flags and output as go crane v0.22.1 for the subset below. One binary
(about 1.7 MiB), no Docker needed.

## Build

```sh
cargo build --release      # binary: target/release/crane
```

## Use

```sh
crane registry serve                        # registry on 127.0.0.1:5050, data in ~/.local/share/crane/registry
crane push image.tar localhost:5050/app:v1  # docker-save tarball or OCI layout dir
crane config localhost:5050/app:v1
crane tag localhost:5050/app:v1 latest
crane ls localhost:5050/app
crane pull localhost:5050/app:v1 out.tar
```

Commands: `push`, `pull`, `tag`, `copy`, `digest`, `manifest`, `config`, `blob`, `ls`,
`catalog`, `delete`, `registry serve`, `version`. Global flags: `--insecure`, `--platform`, `-v`.
Logins come from `auths` in `~/.docker/config.json` (no credential helpers).

`registry serve` keeps blobs, manifests and tags on disk (`--disk DIR`), so it survives
restarts; it listens on loopback unless you pass `--address`.

## Test

```sh
./test.sh          # unit tests
./test.sh --all    # fmt, clippy, all tests incl. cross tests against go crane, size gate
```

The cross tests run {go, rust} client x {go, rust} registry and require identical output;
`scripts/setup-tools.sh` builds the go crane v0.22.1 reference. More: `AGENTS.md`, `PLAN.md`.
