// crane.ts: canonical types for crane-rs (DRAFT, tentative; nothing built from it yet).
//
// crane-rs is a Rust rewrite of a small subset of go-containerregistry's `crane`:
// talk to OCI / Docker v2 registries, and with `crane registry serve`, act as one.
// `crane serve` (no `registry`) is reserved for a later HTTP API over crane's own
// commands; which commands it exposes is still to be decided (deferred ticket).
// Rust types in src/types.rs are derived from this file (ticket-rs pattern:
// ticket.ts -> generate-types.sh -> src/types.rs).
//
// RAW BYTES RULE. A manifest's digest is sha256 over its exact bytes as stored.
// The interfaces below describe the *parsed view* only. crane never re-serializes
// a manifest or config to push, store or print it: it keeps and moves the
// original bytes, and parses them only to read fields.
//
// Reference grammar (same as Docker / go-containerregistry):
//   [registry[:port]/]repository[:tag][@digest]
//   registry defaults to "index.docker.io"; a bare one-segment repository gets "library/";
//   tag defaults to "latest" when there is no digest.
//   e.g. "10.0.0.5:5000/app:v12"
//        "gcr.io/distroless/base-debian13@sha256:8c58c4fa..."
//
// Scheme: https, unless --insecure, or the registry is localhost / 127.0.0.1 / [::1]
// (then http).

// ---------------------------------------------------------------------------
// Identity

/** "sha256:" + 64 lower-case hex chars. Only sha256 is supported. */
export type Digest = string & { readonly __brand: 'digest' }

export interface Reference {
  /** host[:port], e.g. "gcr.io", "localhost:5050", "index.docker.io" */
  registry: string
  /** path, e.g. "library/alpine", "example-app" */
  repository: string
  tag?: string
  digest?: Digest
}

// ---------------------------------------------------------------------------
// OCI / Docker objects (parsed views; see RAW BYTES RULE)

export type MediaType =
  | 'application/vnd.oci.image.manifest.v1+json'
  | 'application/vnd.oci.image.index.v1+json'
  | 'application/vnd.oci.image.config.v1+json'
  | 'application/vnd.oci.image.layer.v1.tar'
  | 'application/vnd.oci.image.layer.v1.tar+gzip'
  | 'application/vnd.oci.image.layer.v1.tar+zstd'
  | 'application/vnd.docker.distribution.manifest.v2+json'
  | 'application/vnd.docker.distribution.manifest.list.v2+json'
  | 'application/vnd.docker.container.image.v1+json'
  | 'application/vnd.docker.image.rootfs.diff.tar.gzip'
  | (string & {}) // artifacts and foreign layers: stored and passed through untouched

export interface Platform {
  os: string
  architecture: string
  variant?: string
  'os.version'?: string
  'os.features'?: string[]
  /** OCI reserved; matched when --platform asks for features. */
  features?: string[]
}

export interface Descriptor {
  mediaType: MediaType
  digest: Digest
  size: number
  urls?: string[]
  annotations?: Record<string, string>
  /** Only on index / manifest-list entries. */
  platform?: Platform
  artifactType?: string
}

/** OCI image manifest or Docker v2 schema 2 manifest (same shape). */
export interface Manifest {
  schemaVersion: 2
  mediaType?: MediaType
  artifactType?: string
  config: Descriptor
  layers: Descriptor[]
  subject?: Descriptor
  annotations?: Record<string, string>
}

/** OCI image index or Docker manifest list (same shape). */
export interface Index {
  schemaVersion: 2
  mediaType?: MediaType
  artifactType?: string
  manifests: Descriptor[]
  subject?: Descriptor
  annotations?: Record<string, string>
}

/** Image config blob. Inner `config` keys are PascalCase on the wire. */
export interface ImageConfig {
  architecture: string
  os: string
  variant?: string
  'os.version'?: string
  created?: string
  author?: string
  config?: {
    User?: string
    Env?: string[]
    Entrypoint?: string[]
    Cmd?: string[]
    WorkingDir?: string
    ExposedPorts?: Record<string, {}>
    Labels?: Record<string, string>
    StopSignal?: string
  }
  rootfs: { type: 'layers'; diff_ids: Digest[] }
  history?: {
    created?: string
    created_by?: string
    comment?: string
    empty_layer?: boolean
  }[]
}

// ---------------------------------------------------------------------------
// Local image formats crane reads and writes

/** One entry of `manifest.json` in a docker-save tarball (what go tarball.WriteToFile writes). */
export interface TarballManifestEntry {
  Config: string
  RepoTags: string[] | null
  Layers: string[]
  /** Foreign (non-distributable) layers, keyed by diff id; omitted when empty. */
  LayerSources?: Record<Digest, Descriptor>
}

/** Accepted by `push`, written by `pull`. Detected from the path, not a flag. */
export type LocalFormat =
  | 'tarball' // docker-save .tar (manifest.json + config + layer files)
  | 'oci' // OCI image layout directory (oci-layout + index.json + blobs/sha256/*)

// ---------------------------------------------------------------------------
// CLI: global options and commands (v1 scope)
//
// Output contract: stdout carries only the result (raw bytes or one value per line),
// so `| jq` and `$(...)` work; progress and warnings go to stderr. Exit 0 on success,
// 1 on any error, with the registry's error code in the message.

export interface GlobalOptions {
  /** Plain HTTP and skip TLS verification. Same flag name as go crane. */
  insecure?: boolean
  /** "os/arch[/variant]". Picks one manifest out of an index; default: keep the whole index. */
  platform?: string
  verbose?: boolean
}

export type Command =
  /** Push a docker-save tarball (gzip layers only) or an OCI layout dir; `index` pushes the
   *  layout's index.json itself. stdout: "<registry>/<repo>@<digest>" */
  | { cmd: 'push'; path: string; ref: string; index?: boolean }
  /** Pull to a tarball (default; digest refs are tagged "i-was-a-digest") or an OCI layout
   *  (whole index unless --platform; `annotateRef` adds org.opencontainers.image.ref.name).
   *  stdout: nothing */
  | { cmd: 'pull'; refs: string[]; path: string; format?: LocalFormat; annotateRef?: boolean }
  /** Add a tag to an existing remote image (manifest re-PUT, no blob copy). stdout: nothing */
  | { cmd: 'tag'; ref: string; tag: string }
  /** Copy an image (or whole index; one image with --platform) between repos/registries,
   *  digest preserved; mounts within a registry. Alias `cp`. stdout: nothing */
  | { cmd: 'copy'; src: string; dst: string }
  /** HEAD for the digest (GET fallback). stdout: "sha256:...\n" or, with fullRef, "<repo>@sha256:...\n" */
  | { cmd: 'digest'; ref: string; fullRef?: boolean }
  /** stdout: raw manifest bytes, no newline (the platform's image with --platform) */
  | { cmd: 'manifest'; ref: string }
  /** stdout: raw config bytes, no newline. An index resolves via --platform, else linux/amd64
   *  (go-containerregistry's default, not the host platform) */
  | { cmd: 'config'; ref: string }
  /** stdout: raw blob bytes, digest verified; ref is "<registry>/<repo>@sha256:..." */
  | { cmd: 'blob'; ref: string }
  /** stdout: one tag per line (full refs with fullRef); omitDigestTags drops "sha256-*" tags */
  | { cmd: 'ls'; repo: string; fullRef?: boolean; omitDigestTags?: boolean }
  /** stdout: one repository per line ("<registry as typed>/<repo>" with fullRef) */
  | { cmd: 'catalog'; registry: string; fullRef?: boolean }
  /** Delete a manifest by tag or digest. stdout: nothing */
  | { cmd: 'delete'; ref: string }
  /** Run a registry; see RegistryServeOptions. Blocks until SIGINT/SIGTERM, then exits 0.
   *  stderr: "listening on http://<addr>" */
  | { cmd: 'registry serve'; options: RegistryServeOptions }
  /** stdout: crane-rs version */
  | { cmd: 'version' }

// ---------------------------------------------------------------------------
// Auth (client side)
//
// Order: `auths` entries in $DOCKER_CONFIG/config.json (default ~/.docker/config.json),
// then anonymous. Bearer flow on 401: parse WWW-Authenticate, GET the realm with
// service + scope, cache the token per (registry, scope) for the process lifetime.
// credsStore / credHelpers: not in v1 (none configured on this Mac today).

export interface DockerConfig {
  auths?: Record<string, { auth?: string; username?: string; password?: string; identitytoken?: string }>
  credsStore?: string
  credHelpers?: Record<string, string>
}

export interface TokenResponse {
  token?: string
  access_token?: string
  expires_in?: number
  issued_at?: string
}

// ---------------------------------------------------------------------------
// crane registry serve: an OCI Distribution v2 registry backed by a directory
//
// Flag names match go crane's `crane registry serve` (--address, --disk, $PORT).
// Two deliberate differences from go crane:
//   - default address is loopback (go: ":0", every interface); LAN needs an explicit --address
//   - storage is always on disk (go: in memory unless --disk); tests pass a temp dir

export interface RegistryServeOptions {
  /** "host:port". Default "127.0.0.1:" + ($PORT or DEFAULT_PORT). Port 0 = pick a free one. */
  address?: string
  /** Storage root. Default DEFAULT_DISK_DIR. One server per root (file lock). */
  disk?: string
}

/**
 * On-disk layout under RegistryServeOptions.disk. Plain files so `ls`, `cat` and `rm` work.
 *   blobs/sha256/<hex>                         content-addressed blob or manifest bytes (shared)
 *   repos/<repo>/_blobs/<hex>                  empty marker: this repo holds this blob
 *                                              (upload or mount creates it; blob DELETE removes only it)
 *   repos/<repo>/_manifests/<hex>              empty marker: this repo holds this manifest
 *   repos/<repo>/_manifests/<hex>.type         its Content-Type (media type)
 *   repos/<repo>/_tags/<tag>                   text file holding a Digest
 *   uploads/<uuid>                             in-progress blob upload (deleted on finish)
 *   serve.lock                                 held while a server runs
 * The "_" prefix: a repo name component can never start with "_", so repo "a/blobs"
 * (repos/a/blobs/) never collides with repo "a"'s markers (repos/a/_blobs/).
 * All writes: write to a temp file (".tmp-*") in the same dir, fsync, rename.
 */
export type StorageLayout = never

/** Routes `crane registry serve` implements (OCI Distribution Spec v1.1). */
export type Route =
  | 'GET /v2/'
  | 'GET|HEAD /v2/<name>/blobs/<digest>'
  | 'DELETE /v2/<name>/blobs/<digest>'
  | 'POST /v2/<name>/blobs/uploads/' // ?digest= (monolithic) or ?mount=&from= (cross-repo)
  | 'PATCH /v2/<name>/blobs/uploads/<uuid>'
  | 'PUT /v2/<name>/blobs/uploads/<uuid>' // ?digest= required
  | 'GET /v2/<name>/blobs/uploads/<uuid>' // upload status: 204 + Range
  | 'DELETE /v2/<name>/blobs/uploads/<uuid>' // cancel upload: 204
  | 'GET|HEAD /v2/<name>/manifests/<reference>'
  | 'PUT /v2/<name>/manifests/<reference>'
  | 'DELETE /v2/<name>/manifests/<reference>'
  | 'GET /v2/<name>/tags/list' // ?n=&last=
  | 'GET /v2/_catalog' // ?n=&last=
  | 'GET /v2/<name>/referrers/<digest>' // ?artifactType=; OCI index of manifests whose subject is <digest>

export type ErrorCode =
  | 'BLOB_UNKNOWN'
  | 'BLOB_UPLOAD_INVALID'
  | 'BLOB_UPLOAD_UNKNOWN'
  | 'DIGEST_INVALID'
  | 'MANIFEST_BLOB_UNKNOWN'
  | 'MANIFEST_INVALID'
  | 'MANIFEST_UNKNOWN'
  | 'NAME_INVALID'
  | 'NAME_UNKNOWN'
  | 'SIZE_INVALID'
  | 'UNAUTHORIZED'
  | 'DENIED'
  | 'UNSUPPORTED'
  | 'TOOMANYREQUESTS'
  | 'UNKNOWN' // crane registry serve: storage I/O failure (HTTP 500)

/** Error body, both served and parsed from remote registries. */
export interface ErrorResponse {
  errors: { code: ErrorCode | (string & {}); message?: string; detail?: unknown }[]
}

export interface TagList {
  name: string
  tags: string[]
}

export interface Catalog {
  repositories: string[]
}

// ---------------------------------------------------------------------------
// Constants

export const DEFAULT_REGISTRY = 'index.docker.io' as const
export const DEFAULT_TAG = 'latest' as const
/** Not 5000: macOS AirPlay Receiver can take 5000. */
export const DEFAULT_PORT = 5050 as const
export const DEFAULT_DISK_DIR = '~/.local/share/crane/registry' as const
/** Header every registry response carries. */
export const API_VERSION_HEADER = 'Docker-Distribution-API-Version: registry/2.0' as const
