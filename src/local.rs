//! Local image formats: docker-save tarballs (what go tarball.WriteToFile writes) and
//! OCI image layout directories. Read for `push`, written by `pull`.
//!
//! Output files go to a temp file that is renamed into place, so a failure or crash never
//! leaves a half-written file under the final name. Like go crane, they are not fsynced:
//! forcing 100s of MB to disk (F_FULLFSYNC on macOS) made `pull` ~1.7x slower than go.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::digest::{self, HashingReader, HashingWriter};
use crate::transfer::{Sink, Source};
use crate::types::{Descriptor, ImageConfig, TarballManifestEntry, docker_manifest_bytes, media};

/// Name, offset and size of each regular file in a tar; symlinks resolved.
fn index_tar(path: &Path) -> Result<HashMap<String, (u64, u64)>> {
    let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut ar = tar::Archive::new(f);
    let mut files = HashMap::new();
    let mut links = Vec::new();
    for entry in ar.entries()? {
        let entry = entry.with_context(|| format!("reading {}", path.display()))?;
        let name = clean(&entry.path()?.to_string_lossy());
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            if let Some(target) = entry.link_name()? {
                links.push((name, clean(&target.to_string_lossy())));
            }
        } else if kind.is_file() {
            files.insert(name, (entry.raw_file_position(), entry.size()));
        }
    }
    for (name, target) in links {
        if let Some(v) = files.get(&target).copied() {
            files.insert(name, v);
        }
    }
    Ok(files)
}

fn clean(name: &str) -> String {
    name.trim_start_matches("./").to_string()
}

/// A single image inside a docker-save tarball, with its Docker schema 2 manifest built
/// exactly as go-containerregistry builds it.
pub struct TarballImage {
    path: PathBuf,
    files: HashMap<String, (u64, u64)>,
    config: Vec<u8>,
    config_digest: String,
    /// layer digest -> file name in the tar
    layer_files: HashMap<String, String>,
    pub manifest: Vec<u8>,
}

impl TarballImage {
    pub fn open(path: &Path) -> Result<Self> {
        let files = index_tar(path)?;
        let mut f = File::open(path)?;
        let mut read_file = |name: &str| -> Result<Vec<u8>> {
            let (off, size) = *files
                .get(name)
                .with_context(|| format!("file {name} not found in tarball"))?;
            f.seek(SeekFrom::Start(off))?;
            let mut buf = vec![0; size as usize];
            f.read_exact(&mut buf)?;
            Ok(buf)
        };
        let entries: Vec<TarballManifestEntry> =
            serde_json::from_slice(&read_file("manifest.json")?)
                .context("parsing manifest.json")?;
        if entries.len() != 1 {
            bail!(
                "tarball must contain only a single image to be used with push (found {})",
                entries.len()
            );
        }
        let entry = &entries[0];
        let config = read_file(&entry.config)?;
        let cfg: ImageConfig = serde_json::from_slice(&config).context("parsing image config")?;
        let config_digest = digest::of(&config);
        let config_desc =
            Descriptor::new(media::DOCKER_CONFIG, config.len() as u64, &config_digest);
        let mut layers = Vec::new();
        let mut layer_files = HashMap::new();
        for (i, name) in entry.layers.iter().enumerate() {
            let diff_id = cfg.rootfs.diff_ids.get(i).with_context(|| {
                format!(
                    "tarball manifest references {} layer(s) but config has {} rootfs.diff_ids",
                    entry.layers.len(),
                    cfg.rootfs.diff_ids.len()
                )
            })?;
            if let Some(d) = entry.layer_sources.get(diff_id) {
                layers.push(d.clone());
                continue;
            }
            let (off, size) = *files
                .get(name)
                .with_context(|| format!("file {name} not found in tarball"))?;
            f.seek(SeekFrom::Start(off))?;
            let mut r = HashingReader::new((&mut f).take(size));
            let mut magic = [0u8; 2];
            r.read_exact(&mut magic)
                .with_context(|| format!("layer {name} is too short"))?;
            if magic != [0x1f, 0x8b] {
                bail!(
                    "layer {name} is not gzip-compressed; uncompressed tarball layers are not supported"
                );
            }
            std::io::copy(&mut r, &mut std::io::sink())?;
            let (d, n) = r.finish();
            layer_files.insert(d.clone(), name.clone());
            layers.push(Descriptor::new(media::DOCKER_LAYER, n, &d));
        }
        let manifest = docker_manifest_bytes(&config_desc, &layers);
        Ok(Self {
            path: path.to_path_buf(),
            files,
            config,
            config_digest,
            layer_files,
            manifest,
        })
    }
}

impl Source for TarballImage {
    fn manifest(&self, digest: &str) -> Result<Vec<u8>> {
        bail!("tarball has no manifest {digest}")
    }
    fn blob(&self, desc: &Descriptor) -> Result<Box<dyn Read + Send>> {
        if desc.digest == self.config_digest {
            return Ok(Box::new(Cursor::new(self.config.clone())));
        }
        let name = self
            .layer_files
            .get(&desc.digest)
            .with_context(|| format!("blob {} not in tarball", desc.digest))?;
        let (off, size) = self.files[name];
        let mut f = File::open(&self.path)?;
        f.seek(SeekFrom::Start(off))?;
        Ok(Box::new(f.take(size)))
    }
}

pub const LAYOUT_FILE: &str = r#"{"imageLayoutVersion": "1.0.0"}"#;

/// index.json, field order as go's v1.IndexManifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexFile {
    #[serde(rename = "schemaVersion")]
    pub schema_version: i64,
    #[serde(rename = "mediaType", default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default)]
    pub manifests: Vec<Descriptor>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

/// An OCI image layout directory.
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    pub fn open(root: &Path) -> Result<Self> {
        if !root.join("oci-layout").is_file() {
            bail!(
                "{} is not an OCI layout (no oci-layout file)",
                root.display()
            );
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// Open, or create an empty layout.
    pub fn create(root: &Path) -> Result<Self> {
        if root.join("oci-layout").is_file() {
            return Self::open(root);
        }
        fs::create_dir_all(root.join("blobs").join("sha256"))?;
        fs::write(root.join("oci-layout"), LAYOUT_FILE)?;
        let l = Self {
            root: root.to_path_buf(),
        };
        l.write_index(&IndexFile {
            schema_version: 2,
            media_type: Some(media::OCI_INDEX.into()),
            manifests: vec![],
            annotations: BTreeMap::new(),
        })?;
        Ok(l)
    }

    pub fn index_bytes(&self) -> Result<Vec<u8>> {
        Ok(fs::read(self.root.join("index.json"))?)
    }

    pub fn index(&self) -> Result<IndexFile> {
        serde_json::from_slice(&self.index_bytes()?).context("parsing index.json")
    }

    pub fn write_index(&self, idx: &IndexFile) -> Result<()> {
        atomic_write(&self.root.join("index.json"), &serde_json::to_vec(idx)?)
    }

    fn blob_path(&self, digest: &str) -> Result<PathBuf> {
        let hex = digest::hex_part(digest).with_context(|| format!("invalid digest {digest}"))?;
        Ok(self.root.join("blobs").join("sha256").join(hex))
    }
}

impl Source for Layout {
    fn manifest(&self, digest: &str) -> Result<Vec<u8>> {
        let p = self.blob_path(digest)?;
        fs::read(&p).with_context(|| format!("reading {}", p.display()))
    }
    fn blob(&self, desc: &Descriptor) -> Result<Box<dyn Read + Send>> {
        let p = self.blob_path(&desc.digest)?;
        Ok(Box::new(
            File::open(&p).with_context(|| format!("opening {}", p.display()))?,
        ))
    }
}

impl Sink for Layout {
    fn has_blob(&self, digest: &str) -> Result<bool> {
        Ok(self.blob_path(digest)?.is_file())
    }
    fn put_blob(&self, desc: &Descriptor, src: &dyn Source) -> Result<()> {
        let path = self.blob_path(&desc.digest)?;
        let tmp = path.with_extension("tmp");
        let mut w = HashingWriter::new(BufWriter::with_capacity(1 << 20, File::create(&tmp)?));
        let res = std::io::copy(&mut src.blob(desc)?, &mut w);
        let (mut f, d, _) = w.finish();
        let res = res.map_err(anyhow::Error::from).and_then(|_| {
            if d != desc.digest {
                bail!("blob digest mismatch: expected {}, got {d}", desc.digest);
            }
            f.flush()?;
            Ok(())
        });
        match res {
            Ok(()) => Ok(fs::rename(&tmp, &path)?),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
    fn put_manifest(&self, _identifier: &str, bytes: &[u8], _content_type: &str) -> Result<String> {
        let d = digest::of(bytes);
        atomic_write(&self.blob_path(&d)?, bytes)?;
        Ok(d)
    }
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Writes a docker-save tarball (go tarball.MultiWriteToFile layout): per image the config
/// as "sha256:<hex>", layers as "<hex>.tar.gz", then manifest.json last.
/// Written to "<path>.tmp" and renamed on `finish`.
pub struct TarballWriter {
    builder: tar::Builder<BufWriter<File>>,
    tmp: PathBuf,
    path: PathBuf,
    written: HashSet<String>,
    entries: Vec<TarballManifestEntry>,
}

impl TarballWriter {
    pub fn create(path: &Path) -> Result<Self> {
        let tmp = PathBuf::from(format!("{}.tmp", path.display()));
        let f = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        Ok(Self {
            builder: tar::Builder::new(BufWriter::with_capacity(1 << 20, f)),
            tmp,
            path: path.to_path_buf(),
            written: HashSet::new(),
            entries: vec![],
        })
    }

    fn append(&mut self, name: &str, size: u64, data: &mut dyn Read) -> Result<()> {
        let mut h = tar::Header::new_ustar();
        h.set_size(size);
        h.set_mode(0o644);
        h.set_mtime(0);
        h.set_entry_type(tar::EntryType::Regular);
        self.builder.append_data(&mut h, name, data)?;
        Ok(())
    }

    /// Add one image. `layers` yields (descriptor, reader) for each layer in order.
    pub fn add_image(
        &mut self,
        tag: &str,
        config: &[u8],
        layers: &[Descriptor],
        open: &mut dyn FnMut(&Descriptor) -> Result<Box<dyn Read + Send>>,
    ) -> Result<()> {
        let config_name = digest::of(config);
        if self.written.insert(config_name.clone()) {
            self.append(&config_name, config.len() as u64, &mut Cursor::new(config))?;
        }
        let mut names = Vec::new();
        for d in layers {
            if d.is_foreign() {
                bail!(
                    "layer {} is a foreign layer; pulling foreign layers is not supported",
                    d.digest
                );
            }
            let hex = digest::hex_part(&d.digest)
                .with_context(|| format!("invalid digest {}", d.digest))?;
            let name = format!("{hex}.tar.gz");
            if self.written.insert(name.clone()) {
                let mut r = HashingReader::new(open(d)?);
                self.append(&name, d.size, &mut r)?;
                let (got, n) = r.finish();
                if got != d.digest || n != d.size {
                    bail!(
                        "layer {} digest or size mismatch (got {got}, {n} bytes)",
                        d.digest
                    );
                }
            }
            names.push(name);
        }
        self.entries.push(TarballManifestEntry {
            config: config_name,
            repo_tags: Some(vec![tag.to_string()]),
            layers: names,
            layer_sources: BTreeMap::new(),
        });
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        let m = serde_json::to_vec(&self.entries)?;
        self.append("manifest.json", m.len() as u64, &mut Cursor::new(m.clone()))?;
        self.builder.into_inner()?.flush()?;
        fs::rename(&self.tmp, &self.path)?;
        Ok(())
    }

    /// Remove the temp file after a failure.
    pub fn abort(self) {
        let _ = fs::remove_file(&self.tmp);
    }
}

#[cfg(test)]
pub(crate) mod tests;
