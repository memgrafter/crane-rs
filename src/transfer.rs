//! Moving manifests and blobs between a Source and a Sink (registry, OCI layout,
//! tarball). push = local -> registry, copy = registry -> registry,
//! pull --format oci = registry -> layout. Bytes are never re-serialized.

use std::collections::HashSet;
use std::io::Read;

use anyhow::{Result, bail};

use crate::client::{Client, Mount};
use crate::digest;
use crate::reference::Repository;
use crate::types::{Descriptor, ManifestView};

pub trait Source {
    /// Raw bytes of a child manifest, by digest.
    fn manifest(&self, digest: &str) -> Result<Vec<u8>>;
    fn blob(&self, desc: &Descriptor) -> Result<Box<dyn Read + Send>>;
    /// The registry repository this source reads from, for cross-repo mounts.
    fn remote(&self) -> Option<&Repository> {
        None
    }
}

pub trait Sink {
    fn has_blob(&self, digest: &str) -> Result<bool>;
    fn put_blob(&self, desc: &Descriptor, src: &dyn Source) -> Result<()>;
    /// Store a manifest under a tag or digest; returns its digest.
    fn put_manifest(&self, identifier: &str, bytes: &[u8], content_type: &str) -> Result<String>;
}

pub struct RemoteRepo<'a> {
    pub client: &'a Client,
    pub repo: Repository,
}

impl Source for RemoteRepo<'_> {
    fn manifest(&self, digest: &str) -> Result<Vec<u8>> {
        Ok(self.client.get_manifest(&self.repo.digest(digest))?.bytes)
    }
    fn blob(&self, desc: &Descriptor) -> Result<Box<dyn Read + Send>> {
        self.client.get_blob(&self.repo, &desc.digest)
    }
    fn remote(&self) -> Option<&Repository> {
        Some(&self.repo)
    }
}

impl Sink for RemoteRepo<'_> {
    fn has_blob(&self, digest: &str) -> Result<bool> {
        self.client.blob_exists(&self.repo, digest)
    }
    fn put_blob(&self, desc: &Descriptor, src: &dyn Source) -> Result<()> {
        let mut upload_url = None;
        if let Some(from) = src
            .remote()
            .filter(|f| f.registry == self.repo.registry && **f != self.repo)
        {
            match self.client.mount(&self.repo, &desc.digest, from)? {
                Mount::Mounted => return Ok(()),
                Mount::Upload(url) => upload_url = Some(url),
            }
        }
        self.client.upload_blob(
            &self.repo,
            &desc.digest,
            desc.size,
            src.blob(desc)?,
            upload_url,
        )
    }
    fn put_manifest(&self, identifier: &str, bytes: &[u8], content_type: &str) -> Result<String> {
        self.client
            .put_manifest(&self.repo, identifier, bytes, content_type)
    }
}

/// Copy the manifest `bytes` (and everything it references) from `src` to `dst`, storing the
/// root under `identifier`. Children are stored by digest. `seen` skips repeated blobs.
pub fn transfer(
    src: &dyn Source,
    dst: &dyn Sink,
    bytes: &[u8],
    content_type: &str,
    identifier: &str,
    seen: &mut HashSet<String>,
) -> Result<String> {
    let view = ManifestView::parse(bytes)?;
    let content_type = view.content_type(content_type).to_string();
    if view.is_index(&content_type) {
        for child in view.manifests.as_deref().unwrap_or_default() {
            let child_bytes = src.manifest(&child.digest)?;
            let got = digest::of(&child_bytes);
            if got != child.digest {
                bail!(
                    "child manifest digest mismatch: expected {}, got {got}",
                    child.digest
                );
            }
            transfer(
                src,
                dst,
                &child_bytes,
                &child.media_type,
                &child.digest,
                seen,
            )?;
        }
    } else {
        for d in view.blobs() {
            if d.is_foreign() || !seen.insert(d.digest.clone()) || dst.has_blob(&d.digest)? {
                continue;
            }
            dst.put_blob(d, src)?;
        }
    }
    dst.put_manifest(identifier, bytes, &content_type)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::Layout;
    use crate::types::media;
    use std::io::Cursor;

    struct Mem(Vec<(String, Vec<u8>)>);
    impl Source for Mem {
        fn manifest(&self, d: &str) -> Result<Vec<u8>> {
            self.0
                .iter()
                .find(|(k, _)| k == d)
                .map(|(_, v)| v.clone())
                .ok_or_else(|| anyhow::anyhow!("no {d}"))
        }
        fn blob(&self, desc: &Descriptor) -> Result<Box<dyn Read + Send>> {
            Ok(Box::new(Cursor::new(self.manifest(&desc.digest)?)))
        }
    }

    fn image(blobs: &[&[u8]]) -> (Vec<u8>, Vec<(String, Vec<u8>)>) {
        let mut store = vec![];
        let descs: Vec<String> = blobs
            .iter()
            .map(|b| {
                store.push((digest::of(b), b.to_vec()));
                format!(
                    r#"{{"mediaType":"l","size":{},"digest":"{}"}}"#,
                    b.len(),
                    digest::of(b)
                )
            })
            .collect();
        let m = format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","config":{},"layers":[{},{{"mediaType":"f","size":1,"digest":"{}","urls":["u"]}}]}}"#,
            media::OCI_MANIFEST,
            descs[0],
            descs[1..].join(","),
            digest::of(b"foreign")
        );
        (m.into_bytes(), store)
    }

    #[test]
    fn image_and_index_into_layout() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::create(dir.path()).unwrap();
        let (m, mut store) = image(&[b"cfg", b"l1", b"l1"]);
        let md = digest::of(&m);
        store.push((md.clone(), m.clone()));
        let idx = format!(
            r#"{{"schemaVersion":2,"manifests":[{{"mediaType":"{}","size":{},"digest":"{md}"}}]}}"#,
            media::OCI_MANIFEST,
            m.len()
        );
        let src = Mem(store);
        assert!(src.remote().is_none());
        let mut seen = HashSet::new();
        let d = transfer(&src, &layout, idx.as_bytes(), "", "tag", &mut seen).unwrap();
        assert_eq!(d, digest::of(idx.as_bytes()));
        for b in [&b"cfg"[..], b"l1", &m, idx.as_bytes()] {
            assert!(layout.has_blob(&digest::of(b)).unwrap());
        }
        assert!(!layout.has_blob(&digest::of(b"foreign")).unwrap());
        // second run: everything already there
        transfer(
            &src,
            &layout,
            idx.as_bytes(),
            "",
            "tag",
            &mut HashSet::new(),
        )
        .unwrap();
    }

    /// Sink that already holds some blobs and counts uploads.
    struct Counting {
        have: HashSet<String>,
        puts: std::cell::Cell<usize>,
    }
    impl Sink for Counting {
        fn has_blob(&self, d: &str) -> Result<bool> {
            Ok(self.have.contains(d))
        }
        fn put_blob(&self, _: &Descriptor, _: &dyn Source) -> Result<()> {
            self.puts.set(self.puts.get() + 1);
            Ok(())
        }
        fn put_manifest(&self, _: &str, bytes: &[u8], _: &str) -> Result<String> {
            Ok(digest::of(bytes))
        }
    }

    #[test]
    fn blobs_already_in_the_sink_are_not_uploaded() {
        let (m, store) = image(&[b"cfg", b"l1", b"l2"]);
        let sink = Counting {
            have: HashSet::from([digest::of(b"l1")]),
            puts: Default::default(),
        };
        transfer(&Mem(store), &sink, &m, "", "t", &mut HashSet::new()).unwrap();
        assert_eq!(sink.puts.get(), 2); // cfg and l2; l1 already there; foreign layer skipped
    }

    #[test]
    fn child_digest_mismatch_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::create(dir.path()).unwrap();
        let wrong = digest::of(b"x");
        let idx = format!(
            r#"{{"mediaType":"{}","manifests":[{{"mediaType":"m","size":1,"digest":"{wrong}"}}]}}"#,
            media::OCI_INDEX
        );
        let src = Mem(vec![(wrong.clone(), b"y".to_vec())]);
        assert!(transfer(&src, &layout, idx.as_bytes(), "", "t", &mut HashSet::new()).is_err());
        assert!(
            transfer(
                &Mem(vec![]),
                &layout,
                idx.as_bytes(),
                "",
                "t",
                &mut HashSet::new()
            )
            .is_err()
        );
        assert!(transfer(&src, &layout, b"not json", "", "t", &mut HashSet::new()).is_err());
    }
}
