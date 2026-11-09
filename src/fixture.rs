//! Deterministic test images: the same seed always gives the same bytes, so every
//! digest in a test run is reproducible. Used by unit, integration, cross and
//! performance tests. Layers are real gzip (stored blocks) of real tar files, so
//! go crane `validate` accepts them.

use std::path::Path;

use anyhow::Result;

use crate::digest;
use crate::types::{Descriptor, TarballManifestEntry, media};

/// IEEE CRC-32 (gzip trailer).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// gzip with stored (uncompressed) deflate blocks, mtime 0: deterministic everywhere.
pub fn gzip_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
    let mut chunks = data.chunks(65535).peekable();
    if chunks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(c) = chunks.next() {
        out.push(u8::from(chunks.peek().is_none()));
        let n = c.len() as u16;
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&(!n).to_le_bytes());
        out.extend_from_slice(c);
    }
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

/// A tar of regular files with fixed metadata.
pub fn tar_of(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (name, data) in files {
        let mut h = tar::Header::new_ustar();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(0);
        h.set_entry_type(tar::EntryType::Regular);
        b.append_data(&mut h, name, *data).expect("in-memory tar");
    }
    b.into_inner().expect("in-memory tar")
}

/// Pseudo-random bytes from a seed (xorshift64*), for large deterministic layers.
pub fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        out.extend_from_slice(&x.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
    }
    out.truncate(len);
    out
}

#[derive(Debug, Clone)]
pub struct Image {
    pub config: Vec<u8>,
    /// gzip-compressed layer blobs
    pub layers: Vec<Vec<u8>>,
    pub os: String,
    pub arch: String,
}

impl Image {
    /// `n_layers` layers of `layer_size` bytes of content each, derived from `seed`.
    pub fn new(seed: u64, n_layers: usize, layer_size: usize, os: &str, arch: &str) -> Self {
        let mut layers = Vec::new();
        let mut diff_ids = Vec::new();
        for i in 0..n_layers {
            let content = noise(seed * 1000 + i as u64, layer_size);
            let tar = tar_of(&[(&format!("data/{seed}-{i}.bin"), &content)]);
            diff_ids.push(format!("\"{}\"", digest::of(&tar)));
            layers.push(gzip_stored(&tar));
        }
        let config = format!(
            r#"{{"architecture":"{arch}","os":"{os}","config":{{"Env":["SEED={seed}"]}},"rootfs":{{"type":"layers","diff_ids":[{}]}}}}"#,
            diff_ids.join(",")
        )
        .into_bytes();
        Self {
            config,
            layers,
            os: os.into(),
            arch: arch.into(),
        }
    }

    pub fn small(seed: u64) -> Self {
        Self::new(seed, 2, 64, "linux", "amd64")
    }

    pub fn config_descriptor(&self, media_type: &str) -> Descriptor {
        Descriptor::new(
            media_type,
            self.config.len() as u64,
            &digest::of(&self.config),
        )
    }

    pub fn layer_descriptors(&self, media_type: &str) -> Vec<Descriptor> {
        self.layers
            .iter()
            .map(|l| Descriptor::new(media_type, l.len() as u64, &digest::of(l)))
            .collect()
    }

    /// OCI image manifest bytes.
    pub fn oci_manifest(&self) -> Vec<u8> {
        let m = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": media::OCI_MANIFEST,
            "config": self.config_descriptor(media::OCI_CONFIG),
            "layers": self.layer_descriptors("application/vnd.oci.image.layer.v1.tar+gzip"),
        });
        serde_json::to_vec(&m).expect("json")
    }
}

/// A docker-save tarball holding `images` (go tarball.MultiWriteToFile naming).
pub fn docker_tarball(images: &[(&str, &Image)]) -> Vec<u8> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut entries = Vec::new();
    for (tag, img) in images {
        let cfg_name = digest::of(&img.config);
        files.push((cfg_name.clone(), img.config.clone()));
        let mut names = Vec::new();
        for l in &img.layers {
            let name = format!("{}.tar.gz", &digest::of(l)[7..]);
            files.push((name.clone(), l.clone()));
            names.push(name);
        }
        entries.push(TarballManifestEntry {
            config: cfg_name,
            repo_tags: Some(vec![tag.to_string()]),
            layers: names,
            layer_sources: Default::default(),
        });
    }
    files.push((
        "manifest.json".into(),
        serde_json::to_vec(&entries).expect("json"),
    ));
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_slice()))
        .collect();
    tar_of(&refs)
}

/// Write an OCI layout at `dir`. With `as_index`, index.json points at one OCI index
/// holding every image (with platforms); otherwise index.json lists the images.
pub fn oci_layout(dir: &Path, images: &[&Image], as_index: bool) -> Result<String> {
    let blobs = dir.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs)?;
    std::fs::write(dir.join("oci-layout"), crate::local::LAYOUT_FILE)?;
    let put = |b: &[u8]| -> Result<String> {
        let d = digest::of(b);
        std::fs::write(blobs.join(&d[7..]), b)?;
        Ok(d)
    };
    let mut descs = Vec::new();
    for img in images {
        put(&img.config)?;
        for l in &img.layers {
            put(l)?;
        }
        let m = img.oci_manifest();
        let mut d = Descriptor::new(media::OCI_MANIFEST, m.len() as u64, &put(&m)?);
        d.platform = Some(crate::types::Platform {
            os: img.os.clone(),
            architecture: img.arch.clone(),
            ..Default::default()
        });
        descs.push(d);
    }
    let top = if as_index {
        let idx = serde_json::to_vec(
            &serde_json::json!({"schemaVersion": 2, "mediaType": media::OCI_INDEX, "manifests": descs}),
        )
        .expect("json");
        vec![Descriptor::new(
            media::OCI_INDEX,
            idx.len() as u64,
            &put(&idx)?,
        )]
    } else {
        descs
    };
    let index = serde_json::to_vec(
        &serde_json::json!({"schemaVersion": 2, "mediaType": media::OCI_INDEX, "manifests": top}),
    )
    .expect("json");
    std::fs::write(dir.join("index.json"), &index)?;
    Ok(top[0].digest.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN_NOISE: &str = "7a48219ae2b3830d679dfef1794cc454";
    const GOLDEN_NOISE_BIG: &str =
        "sha256:0917805753ffce8df8bf3b651e45554ba73e6f4d40285e474ab28556dddb7851";

    #[test]
    fn crc_and_gzip_known_values() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        // gzip of empty input: header, final empty stored block, crc 0, size 0
        assert_eq!(
            gzip_stored(b""),
            [
                0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255, 1, 0, 0, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 0
            ]
        );
        let big = noise(1, 70_000);
        let g = gzip_stored(&big);
        // two stored blocks: first not final, second final
        assert_eq!(g[10], 0);
        assert_eq!(g[10 + 5 + 65535], 1);
        assert_eq!(g.len(), 10 + 5 + 65535 + 5 + (70_000 - 65535) + 8);
    }

    #[test]
    fn golden_bytes() {
        // Pinned outputs: any change to the generators changes every fixture digest.
        assert_eq!(crate::digest::hex(&noise(1, 16)), GOLDEN_NOISE);
        assert_eq!(crate::digest::of(&noise(42, 100_003)), GOLDEN_NOISE_BIG);
        assert_eq!(
            crate::digest::hex(&gzip_stored(b"abc")),
            "1f8b08000000000000ff010300fcff616263c241243503000000"
        );
        let dir = tempfile::tempdir().unwrap();
        oci_layout(
            dir.path(),
            &[&Image::new(2, 1, 10, "linux", "arm64")],
            false,
        )
        .unwrap();
        let idx = std::fs::read_to_string(dir.path().join("index.json")).unwrap();
        assert!(
            idx.contains(r#""platform":{"architecture":"arm64","os":"linux"}"#),
            "{idx}"
        );
    }

    #[test]
    fn deterministic() {
        assert_eq!(noise(7, 33), noise(7, 33));
        assert_ne!(noise(7, 33), noise(8, 33));
        assert_eq!(noise(7, 33).len(), 33);
        let a = Image::small(3);
        let b = Image::small(3);
        assert_eq!(a.oci_manifest(), b.oci_manifest());
        assert_eq!(
            docker_tarball(&[("x:y", &a)]),
            docker_tarball(&[("x:y", &b)])
        );
        assert_ne!(Image::small(4).config, a.config);
    }

    #[test]
    fn layouts() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (Image::small(1), Image::new(2, 1, 10, "linux", "arm64"));
        let d = oci_layout(dir.path(), &[&a, &b], true).unwrap();
        assert!(dir.path().join("blobs/sha256").join(&d[7..]).is_file());
        let dir2 = tempfile::tempdir().unwrap();
        let d2 = oci_layout(dir2.path(), &[&a], false).unwrap();
        assert_eq!(d2, digest::of(&a.oci_manifest()));
    }
}
