//! Serde types from crane.ts. Parsed views only: crane keeps and moves original
//! manifest / config bytes and never re-serializes them (RAW BYTES RULE in crane.ts).
//! The one exception is `docker_manifest_bytes`, which builds the manifest for a
//! docker-save tarball exactly as go-containerregistry's json.Marshal does.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub mod media {
    pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
    pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
    pub const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
    pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
    pub const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
    pub const DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";
    pub const DOCKER_LAYER: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
    /// Accept header for manifest requests, most specific first.
    pub const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

    pub fn is_index(media_type: &str) -> bool {
        media_type == OCI_INDEX || media_type == DOCKER_LIST
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Platform {
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub os: String,
    #[serde(
        rename = "os.version",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub os_version: String,
    #[serde(rename = "os.features", default, skip_serializing_if = "Vec::is_empty")]
    pub os_features: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub variant: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

impl Platform {
    /// "os/arch[/variant][:osversion]", as go crane's --platform.
    pub fn parse(s: &str) -> Result<Self> {
        let (main, os_version) = match s.split_once(':') {
            Some((m, v)) => (m, v.to_string()),
            None => (s, String::new()),
        };
        let parts: Vec<&str> = main.split('/').collect();
        if parts.len() < 2 || parts.len() > 3 || parts.iter().any(|p| p.is_empty()) {
            bail!("platform must be os/arch[/variant][:osversion]: {s}");
        }
        Ok(Self {
            os: parts[0].to_string(),
            architecture: parts[1].to_string(),
            variant: parts.get(2).map(|v| v.to_string()).unwrap_or_default(),
            os_version,
            ..Default::default()
        })
    }

    /// go remote's default when no platform is given.
    pub fn default_linux_amd64() -> Self {
        Self {
            os: "linux".into(),
            architecture: "amd64".into(),
            ..Default::default()
        }
    }

    /// Does `self` (an index entry) satisfy `want`? OS and arch must match; variant and
    /// os.version only when `want` sets them; `want`'s features must all be present.
    pub fn satisfies(&self, want: &Platform) -> bool {
        self.os == want.os
            && self.architecture == want.architecture
            && (want.variant.is_empty() || self.variant == want.variant)
            && (want.os_version.is_empty() || self.os_version == want.os_version)
            && want.features.iter().all(|f| self.features.contains(f))
            && want
                .os_features
                .iter()
                .all(|f| self.os_features.contains(f))
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.os, self.architecture)?;
        if !self.variant.is_empty() {
            write!(f, "/{}", self.variant)?;
        }
        if !self.os_version.is_empty() {
            write!(f, ":{}", self.os_version)?;
        }
        Ok(())
    }
}

/// Field order matches go v1.Descriptor so serialized descriptors are byte-identical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Descriptor {
    #[serde(rename = "mediaType", default)]
    pub media_type: String,
    pub size: u64,
    pub digest: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
    #[serde(
        rename = "artifactType",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub artifact_type: Option<String>,
}

impl Descriptor {
    pub fn new(media_type: &str, size: u64, digest: &str) -> Self {
        Self {
            media_type: media_type.to_string(),
            size,
            digest: digest.to_string(),
            urls: Vec::new(),
            annotations: BTreeMap::new(),
            platform: None,
            artifact_type: None,
        }
    }

    /// Foreign (non-distributable) layers carry URLs and are not pushed.
    pub fn is_foreign(&self) -> bool {
        !self.urls.is_empty()
    }
}

/// Parsed view of either an image manifest or an index.
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestView {
    #[serde(rename = "mediaType", default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub config: Option<Descriptor>,
    #[serde(default)]
    pub layers: Vec<Descriptor>,
    #[serde(default)]
    pub manifests: Option<Vec<Descriptor>>,
    #[serde(default)]
    pub subject: Option<Descriptor>,
}

impl ManifestView {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }

    /// Index if the Content-Type or mediaType says so, or (no media type) it has `manifests`.
    pub fn is_index(&self, content_type: &str) -> bool {
        if media::is_index(content_type) {
            return true;
        }
        match &self.media_type {
            Some(mt) => media::is_index(mt),
            None => self.manifests.is_some() && self.config.is_none(),
        }
    }

    /// The media type to send as Content-Type: the manifest's own, else the server's.
    pub fn content_type<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.media_type
            .as_deref()
            .filter(|m| !m.is_empty())
            .unwrap_or(fallback)
    }

    /// Pick the index entry for a platform (first match, as go does).
    pub fn pick(&self, want: &Platform) -> Result<&Descriptor> {
        let manifests = self.manifests.as_deref().unwrap_or_default();
        manifests
            .iter()
            .find(|d| d.platform.as_ref().is_some_and(|p| p.satisfies(want)))
            .ok_or_else(|| anyhow::anyhow!("no child with platform {want} in index"))
    }

    /// Blobs an image manifest needs: config then layers.
    pub fn blobs(&self) -> Vec<&Descriptor> {
        self.config.iter().chain(self.layers.iter()).collect()
    }
}

/// The parts of an image config crane reads.
#[derive(Debug, Clone, Deserialize)]
pub struct ImageConfig {
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub variant: String,
    #[serde(rename = "os.version", default)]
    pub os_version: String,
    #[serde(default)]
    pub rootfs: RootFs,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RootFs {
    #[serde(default)]
    pub diff_ids: Vec<String>,
}

impl ImageConfig {
    pub fn platform(&self) -> Platform {
        Platform {
            os: self.os.clone(),
            architecture: self.architecture.clone(),
            variant: self.variant.clone(),
            os_version: self.os_version.clone(),
            ..Default::default()
        }
    }
}

/// One entry of `manifest.json` in a docker-save tarball.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TarballManifestEntry {
    #[serde(rename = "Config")]
    pub config: String,
    #[serde(rename = "RepoTags")]
    pub repo_tags: Option<Vec<String>>,
    #[serde(rename = "Layers")]
    pub layers: Vec<String>,
    /// Foreign layers, keyed by diff id.
    #[serde(
        rename = "LayerSources",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub layer_sources: BTreeMap<String, Descriptor>,
}

#[derive(Serialize)]
struct DockerManifest<'a> {
    #[serde(rename = "schemaVersion")]
    schema_version: i64,
    #[serde(rename = "mediaType")]
    media_type: &'a str,
    config: &'a Descriptor,
    layers: &'a [Descriptor],
}

/// Docker schema 2 manifest bytes, identical to go's tarball.Image + json.Marshal.
/// (go escapes '<', '>' and '&' in strings; those only occur in foreign-layer URLs,
/// which are escaped the same way below.)
pub fn docker_manifest_bytes(config: &Descriptor, layers: &[Descriptor]) -> Vec<u8> {
    let m = DockerManifest {
        schema_version: 2,
        media_type: media::DOCKER_MANIFEST,
        config,
        layers,
    };
    let s = serde_json::to_string(&m).expect("manifest serializes");
    s.replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .into_bytes()
}

/// OCI / registry error body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorResponse {
    pub errors: Vec<ErrorItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorItem {
    pub code: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

impl ErrorResponse {
    /// "CODE: message; CODE2: message2"
    pub fn summary(&self) -> String {
        self.errors
            .iter()
            .map(|e| {
                if e.message.is_empty() {
                    e.code.clone()
                } else {
                    format!("{}: {}", e.code, e.message)
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TagList {
    pub name: String,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Catalog {
    #[serde(default)]
    pub repositories: Option<Vec<String>>,
}

/// Field names checked against crane.ts.
#[cfg(test)]
#[path = "types/tests.rs"]
mod crane_ts;

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn platform_parse_and_display() {
        let p = Platform::parse("linux/arm64/v8:10.0").unwrap();
        assert_eq!((p.os.as_str(), p.architecture.as_str()), ("linux", "arm64"));
        assert_eq!((p.variant.as_str(), p.os_version.as_str()), ("v8", "10.0"));
        assert_eq!(p.to_string(), "linux/arm64/v8:10.0");
        assert_eq!(
            Platform::parse("linux/amd64").unwrap(),
            Platform::default_linux_amd64()
        );
        assert_eq!(Platform::default_linux_amd64().to_string(), "linux/amd64");
        for bad in ["linux", "linux/", "/amd64", "a/b/c/d", ""] {
            assert!(Platform::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn platform_matching() {
        let have = Platform {
            os: "linux".into(),
            architecture: "arm".into(),
            variant: "v7".into(),
            os_version: "1".into(),
            features: vec!["f".into()],
            os_features: vec!["g".into()],
        };
        assert!(have.satisfies(&Platform::parse("linux/arm").unwrap()));
        assert!(have.satisfies(&Platform::parse("linux/arm/v7:1").unwrap()));
        assert!(!have.satisfies(&Platform::parse("linux/arm/v6").unwrap()));
        assert!(!have.satisfies(&Platform::parse("linux/arm:2").unwrap()));
        assert!(!have.satisfies(&Platform::parse("windows/arm").unwrap()));
        assert!(!have.satisfies(&Platform::parse("linux/amd64").unwrap()));
        let mut want = Platform::parse("linux/arm").unwrap();
        want.features = vec!["f".into()];
        want.os_features = vec!["g".into()];
        assert!(have.satisfies(&want));
        want.features = vec!["x".into()];
        assert!(!have.satisfies(&want));
        want.features.clear();
        want.os_features = vec!["x".into()];
        assert!(!have.satisfies(&want));
    }

    #[test]
    fn manifest_view_image_and_index() {
        let img = format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","config":{{"mediaType":"c","size":1,"digest":"{D}"}},"layers":[{{"mediaType":"l","size":2,"digest":"{D}","urls":["u"]}}]}}"#,
            media::OCI_MANIFEST
        );
        let v = ManifestView::parse(img.as_bytes()).unwrap();
        assert!(!v.is_index(""));
        assert!(v.is_index(media::OCI_INDEX));
        assert_eq!(v.content_type("x"), media::OCI_MANIFEST);
        assert_eq!(v.blobs().len(), 2);
        assert!(v.layers[0].is_foreign());
        assert!(v.subject.is_none());

        let idx = format!(
            r#"{{"schemaVersion":2,"manifests":[{{"mediaType":"m","size":3,"digest":"{D}","platform":{{"os":"linux","architecture":"arm64"}}}},{{"mediaType":"m","size":3,"digest":"{D}"}}]}}"#
        );
        let v = ManifestView::parse(idx.as_bytes()).unwrap();
        assert!(v.is_index(""));
        assert_eq!(v.content_type("fallback"), "fallback");
        assert_eq!(
            v.pick(&Platform::parse("linux/arm64").unwrap())
                .unwrap()
                .size,
            3
        );
        assert!(v.pick(&Platform::default_linux_amd64()).is_err());

        let docker_list = format!(r#"{{"mediaType":"{}","manifests":[]}}"#, media::DOCKER_LIST);
        assert!(
            ManifestView::parse(docker_list.as_bytes())
                .unwrap()
                .is_index("")
        );
        let empty_mt = r#"{"mediaType":"","config":null}"#;
        let v = ManifestView::parse(empty_mt.as_bytes()).unwrap();
        assert_eq!(v.content_type("f"), "f");
        assert!(!v.is_index(""));
        assert!(v.pick(&Platform::default_linux_amd64()).is_err());
        assert!(ManifestView::parse(b"not json").is_err());
    }

    #[test]
    fn is_index_needs_manifests_without_config() {
        let both =
            format!(r#"{{"config":{{"mediaType":"c","size":1,"digest":"{D}"}},"manifests":[]}}"#);
        assert!(!ManifestView::parse(both.as_bytes()).unwrap().is_index(""));
        assert!(
            ManifestView::parse(br#"{"manifests":[]}"#)
                .unwrap()
                .is_index("")
        );
        assert!(!ManifestView::parse(br#"{}"#).unwrap().is_index(""));
    }

    #[test]
    fn docker_manifest_matches_go_marshal() {
        let config = Descriptor::new(media::DOCKER_CONFIG, 10, D);
        let mut foreign = Descriptor::new(
            "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip",
            5,
            D,
        );
        foreign.urls = vec!["https://x/?a=1&b=<2>".into()];
        let layers = vec![Descriptor::new(media::DOCKER_LAYER, 20, D), foreign];
        let got = String::from_utf8(docker_manifest_bytes(&config, &layers)).unwrap();
        let want = format!(
            concat!(
                r#"{{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","#,
                r#""config":{{"mediaType":"application/vnd.docker.container.image.v1+json","size":10,"digest":"{d}"}},"#,
                r#""layers":[{{"mediaType":"application/vnd.docker.image.rootfs.diff.tar.gzip","size":20,"digest":"{d}"}},"#,
                r#"{{"mediaType":"application/vnd.docker.image.rootfs.foreign.diff.tar.gzip","size":5,"digest":"{d}","urls":["https://x/?a=1\u0026b=\u003c2\u003e"]}}]}}"#
            ),
            d = D
        );
        assert_eq!(got, want);
    }

    #[test]
    fn image_config_platform() {
        let c: ImageConfig = serde_json::from_str(
            r#"{"architecture":"arm","os":"linux","variant":"v7","rootfs":{"type":"layers","diff_ids":["x"]}}"#,
        )
        .unwrap();
        assert_eq!(c.platform().to_string(), "linux/arm/v7");
        assert_eq!(c.rootfs.diff_ids, vec!["x"]);
    }

    #[test]
    fn image_config_os_version() {
        let c: ImageConfig = serde_json::from_str(
            r#"{"architecture":"amd64","os":"windows","os.version":"10.0.1"}"#,
        )
        .unwrap();
        assert_eq!(c.platform().to_string(), "windows/amd64:10.0.1");
    }

    #[test]
    fn tarball_entry_round_trip() {
        let e: TarballManifestEntry =
            serde_json::from_str(r#"[{"Config":"c","RepoTags":null,"Layers":["a"]}]"#)
                .map(|v: Vec<TarballManifestEntry>| v.into_iter().next().unwrap())
                .unwrap();
        assert_eq!(e.config, "c");
        assert!(e.repo_tags.is_none());
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"Config":"c","RepoTags":null,"Layers":["a"]}"#
        );
    }

    #[test]
    fn error_summary_and_lists() {
        let e: ErrorResponse = serde_json::from_str(
            r#"{"errors":[{"code":"MANIFEST_UNKNOWN","message":"nope"},{"code":"DENIED"}]}"#,
        )
        .unwrap();
        assert_eq!(e.summary(), "MANIFEST_UNKNOWN: nope; DENIED");
        let t: TagList = serde_json::from_str(r#"{"name":"a","tags":null}"#).unwrap();
        assert_eq!(t.tags, None);
        let c: Catalog = serde_json::from_str(r#"{"repositories":["a"]}"#).unwrap();
        assert_eq!(c.repositories, Some(vec!["a".to_string()]));
    }
}
