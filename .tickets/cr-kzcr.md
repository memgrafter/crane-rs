---
id: cr-kzcr
status: deferred
open: true
deps: []
links: []
created: 2026-11-09T14:19:03Z
type: feature
priority: 3
assignee: memgrafter
parent: cr-df8g
tags: [crane, container, idea]
---
# crane container image (crane in a container): tradeoffs, deferred

User idea (2026-11-09): a container image that contains crane ("crane crane container"), used to run crane. The user asked what it offers when we have no Go toolchain, and what the tradeoffs are. Deferred; nothing built.

What it is: an OCI image holding the static crane-rs binary (FROM scratch is enough: crane-rs embeds its TLS root certificates via webpki-roots, so it needs no CA files), multi-arch (linux/amd64 + linux/arm64), built and pushed with crane itself.

What it offers
- Running crane where only containers run: k3s Jobs/CronJobs (e.g. mirror gcr.io/distroless into the cluster registry), init containers, CI systems whose steps are images.
- `crane registry serve` as a Deployment (a dev or pull-through registry in the cluster), with a volume for --disk and an explicit --address 0.0.0.0:5050 (decision D3).
- Dogfooding: crane builds and pushes its own image, and the cluster pulls it, which tests push, index and pull end to end.
- Install-free use on a machine with a container runtime: one `docker run` / `nerdctl run`, no Rust or Go toolchain.

What it does not offer here
- On this Mac there is no container runtime (builds run on the Mac with no Docker or Colima), so the image cannot run locally; the native binary is the way to run crane here.
- It is not the answer to "no Go toolchain" for the test oracle: downloading go-containerregistry's release binary for v0.22.1 and checking its sha256 needs no Go and no container runtime. That is the better fallback for scripts/setup-tools.sh if Go is missing.
- A static Rust binary already has no runtime dependencies, so the container adds packaging, not portability.

Costs
- A Linux build: static musl cross-compile for amd64 + arm64 (replmux has the pattern: scripts/lib-cross.sh, release.sh --static).
- Building an image needs an image-building step crane-rs does not have in v1 (go crane's `append` / `mutate`, or a small image generator). Options: add `crane append` to crane-rs, or write the image layout with crate code (crane::fixture already writes tars, gzip and OCI layouts).
- One more release artifact to version and keep in step with the binary.

Recommendation: wait until there is a cluster use (mirror job or an in-cluster `crane registry serve`). Then: static musl builds -> one-layer image FROM scratch -> multi-arch index -> `crane push` to the cluster registry.

**Start trigger** (user, 2026-11-09: keep it ticketed, deferred until a cluster use): start when one of these is wanted: (1) a k3s Job/CronJob that mirrors an image (e.g. gcr.io/distroless/base-debian13:nonroot) into the cluster registry; (2) `crane registry serve` as an in-cluster Deployment; (3) a CI system whose steps are images.

**Facts checked 2026-11-09**
- TLS roots are compiled in (webpki-roots is in Cargo.lock), so FROM scratch needs no CA files.
- In a FROM scratch image $HOME is usually unset: `crane registry serve` then stops with "no --disk given and $HOME is not set" (src/registry/mod.rs storage_root), so the Deployment must pass --disk (a volume) and --address 0.0.0.0:5050.
- The static build pattern exists in replmux/scripts/release.sh --static (musl) and scripts/lib-cross.sh.
- Builds run on the Mac, no Docker or Colima, so the image cannot be run on this Mac.
- crane-rs has no image-building command (no append/mutate); crane::fixture already writes tar, gzip and OCI layouts.
- If cr-888t (fewer deps) lands first, the image gets the smaller binary; nothing here blocks it.

**Steps when started**
1. Static musl builds for x86_64 and aarch64 Linux (copy replmux's release.sh --static pattern into scripts/release.sh).
2. Write a one-layer OCI layout per arch (binary at /crane, entrypoint ["/crane"], config os/arch set) plus an index, with a small example or command built on crane::fixture code. Reproducible: mtime 0, fixed order, so the same binary gives the same digest.
3. `crane push --index <layout> <registry>/crane:<version>` to the cluster registry.
4. Use it in the triggering Job or Deployment (manifest in the cluster config repo, not here).

## Acceptance Criteria

- Two builds from the same commit give the same image index digest.
- go crane v0.22.1 `validate --remote` accepts the pushed index; `crane config --platform linux/amd64` and `linux/arm64` show the right os/architecture.
- The image runs `crane version` in the cluster (a one-off Job) on both architectures present there.
- For a registry Deployment: pushes and pulls from the Mac to the in-cluster `crane registry serve` work, and data survives a pod restart (volume on --disk).
