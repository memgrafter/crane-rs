//! Plain-file registry storage (crane.ts `StorageLayout`):
//!
//! ```text
//! blobs/sha256/<hex>                  blob or manifest bytes, shared by all repos
//! repos/<repo>/_blobs/<hex>           empty marker: the repo holds this blob
//! repos/<repo>/_manifests/<hex>       empty marker: the repo holds this manifest
//! repos/<repo>/_manifests/<hex>.type  the manifest's Content-Type
//! repos/<repo>/_tags/<tag>            text file holding a digest
//! uploads/<id>                        in-progress blob upload
//! ```
//!
//! The `_` prefix keeps these apart from nested repo names: a name component
//! can never start with `_`, so `repos/a/_blobs` and repo `a/blobs` never meet.
//! Every write goes to a temp file (`.tmp-*`) in the target directory, is
//! fsynced, then renamed into place, so readers never see a torn file.
//! Callers validate names, tags, digests and upload ids before calling in.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::digest::{self, Hasher};

const BLOBS: &str = "_blobs";
const MANIFESTS: &str = "_manifests";
const TAGS: &str = "_tags";

/// A fresh id: 32 lower-case hex chars, unique within this process and very
/// likely across processes (pid, clock and a counter, hashed).
pub fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seed = format!(
        "{}-{}-{}",
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    digest::of(seed.as_bytes())[digest::PREFIX.len()..][..32].to_string()
}

/// An upload id as made by `new_id`.
pub fn is_upload_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_hex64(name: &str) -> bool {
    digest::hex_part(&format!("{}{name}", digest::PREFIX)).is_some()
}

/// Hand a file's data to the storage device before it is renamed into place.
///
/// This is fsync(2). On Linux that is what `File::sync_all` does. On macOS `sync_all` uses
/// F_FULLFSYNC, which also flushes the drive's own cache and cost ~3 ms per call: pushing
/// 200 small blobs took 1.18 s vs 0.14 s with fsync(2) (go's registry: 0.13 s, no sync).
/// Trade-off: on macOS a power cut can still lose the newest blobs; a process or OS crash
/// cannot. Clients verify digests, so a lost or damaged blob is detected and re-pushed.
pub(crate) fn durable(f: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: fsync on a valid, open file descriptor.
        let rc = unsafe { libc::fsync(f.as_raw_fd()) };
        (rc == 0).then_some(()).ok_or_else(io::Error::last_os_error)
    }
    #[cfg(not(target_vendor = "apple"))]
    f.sync_all()
}

/// Write `bytes` to `path` atomically: temp file in the same dir, fsync, rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".tmp-{}", new_id()));
    let res = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        durable(&f)?;
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

/// `Ok(None)` for NotFound, other errors unchanged.
fn found<T>(r: io::Result<T>) -> io::Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Remove a file; false if it was not there.
fn remove(path: &Path) -> io::Result<bool> {
    Ok(found(fs::remove_file(path))?.is_some())
}

/// Entry names of a directory; empty if it does not exist.
fn names(dir: &Path) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    if let Some(entries) = found(fs::read_dir(dir))? {
        for entry in entries {
            out.push(entry?.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(out)
}

/// A stored manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub digest: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

pub struct Store {
    root: PathBuf,
    /// Running hash of each upload, valid while its length matches the file.
    hashers: Mutex<HashMap<String, Hasher>>,
}

impl Store {
    /// Open (creating the top-level dirs of) a store at `root`.
    pub fn open(root: &Path) -> io::Result<Store> {
        for d in ["blobs/sha256", "repos", "uploads"] {
            fs::create_dir_all(root.join(d))?;
        }
        Ok(Store {
            root: root.to_path_buf(),
            hashers: Mutex::new(HashMap::new()),
        })
    }

    fn hashers(&self) -> std::sync::MutexGuard<'_, HashMap<String, Hasher>> {
        self.hashers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn blob_path(&self, hex: &str) -> PathBuf {
        self.root.join("blobs/sha256").join(hex)
    }

    fn repo_dir(&self, repo: &str) -> PathBuf {
        self.root.join("repos").join(repo)
    }

    fn upload_path(&self, id: &str) -> PathBuf {
        self.root.join("uploads").join(id)
    }

    fn marker(&self, repo: &str, kind: &str, digest: &str) -> Option<PathBuf> {
        let hex = digest::hex_part(digest)?;
        Some(self.repo_dir(repo).join(kind).join(hex))
    }

    // ---- repos

    /// A repo exists once it holds any blob or manifest.
    pub fn repo_exists(&self, repo: &str) -> io::Result<bool> {
        for kind in [BLOBS, MANIFESTS] {
            if names(&self.repo_dir(repo).join(kind))?
                .iter()
                .any(|n| is_hex64(n))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// All repos, sorted.
    pub fn repos(&self) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        self.walk_repos(&self.root.join("repos"), "", &mut out)?;
        out.sort();
        Ok(out)
    }

    fn walk_repos(&self, dir: &Path, prefix: &str, out: &mut Vec<String>) -> io::Result<()> {
        for name in names(dir)? {
            if name.starts_with(['_', '.']) || !dir.join(&name).is_dir() {
                continue;
            }
            let repo = format!("{prefix}{name}");
            if self.repo_exists(&repo)? {
                out.push(repo.clone());
            }
            self.walk_repos(&dir.join(&name), &format!("{repo}/"), out)?;
        }
        Ok(())
    }

    // ---- blobs

    pub fn has_blob(&self, digest: &str) -> bool {
        digest::hex_part(digest).is_some_and(|h| self.blob_path(h).is_file())
    }

    pub fn has_blob_link(&self, repo: &str, digest: &str) -> bool {
        self.marker(repo, BLOBS, digest)
            .is_some_and(|p| p.is_file() && self.has_blob(digest))
    }

    /// Mark `digest` (which must be stored) as held by `repo`.
    pub fn link_blob(&self, repo: &str, digest: &str) -> io::Result<()> {
        let path = self
            .marker(repo, BLOBS, digest)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad digest"))?;
        write_atomic(&path, b"")
    }

    pub fn unlink_blob(&self, repo: &str, digest: &str) -> io::Result<bool> {
        match self.marker(repo, BLOBS, digest) {
            Some(p) => remove(&p),
            None => Ok(false),
        }
    }

    /// The blob's file and size, if `repo` holds it.
    pub fn open_blob(&self, repo: &str, digest: &str) -> io::Result<Option<(File, u64)>> {
        if !self
            .marker(repo, BLOBS, digest)
            .is_some_and(|p| p.is_file())
        {
            return Ok(None);
        }
        let hex = digest::hex_part(digest).unwrap_or_default();
        let Some(f) = found(File::open(self.blob_path(hex)))? else {
            return Ok(None);
        };
        let len = f.metadata()?.len();
        Ok(Some((f, len)))
    }

    // ---- uploads

    pub fn new_upload(&self) -> io::Result<String> {
        let id = new_id();
        File::create(self.upload_path(&id))?;
        self.hashers().insert(id.clone(), Hasher::new());
        Ok(id)
    }

    /// Bytes received so far, or None for an unknown upload.
    pub fn upload_len(&self, id: &str) -> io::Result<Option<u64>> {
        Ok(found(fs::metadata(self.upload_path(id)))?.map(|m| m.len()))
    }

    /// The running hash for an upload whose file is `len` bytes long, re-hashing
    /// the file if the cached state is missing or stale (e.g. after a restart).
    fn hasher_for(&self, id: &str, len: u64) -> io::Result<Hasher> {
        if let Some(h) = self.hashers().remove(id).filter(|h| h.len() == len) {
            return Ok(h);
        }
        let mut h = Hasher::new();
        let mut f = File::open(self.upload_path(id))?;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                return Ok(h);
            }
            h.update(&buf[..n]);
        }
    }

    /// Append `body` to an upload, hashing as it streams. New length, or None
    /// for an unknown upload.
    pub fn append_upload(&self, id: &str, body: &mut dyn Read) -> io::Result<Option<u64>> {
        let Some(mut f) = found(OpenOptions::new().append(true).open(self.upload_path(id)))? else {
            return Ok(None);
        };
        let mut h = self.hasher_for(id, f.metadata()?.len())?;
        let mut buf = vec![0u8; 64 * 1024];
        let res = loop {
            match body.read(&mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => {
                    f.write_all(&buf[..n])?;
                    h.update(&buf[..n]);
                }
                Err(e) => break Err(e),
            }
        };
        // Keep what was written so the client can resume from Range.
        let len = h.len();
        self.hashers().insert(id.to_string(), h);
        res.map(|()| Some(len))
    }

    /// ("sha256:<hex>", size) of an upload's bytes, or None if unknown.
    pub fn upload_digest(&self, id: &str) -> io::Result<Option<(String, u64)>> {
        let Some(len) = self.upload_len(id)? else {
            return Ok(None);
        };
        let h = self.hasher_for(id, len)?;
        self.hashers().insert(id.to_string(), h.clone());
        Ok(Some(h.finish()))
    }

    /// Move a verified upload into blobs/ (or drop it if the blob is already stored).
    pub fn commit_upload(&self, id: &str, digest: &str) -> io::Result<()> {
        let hex = digest::hex_part(digest)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad digest"))?;
        let path = self.upload_path(id);
        self.hashers().remove(id);
        if self.blob_path(hex).is_file() {
            return fs::remove_file(path);
        }
        durable(&File::open(&path)?)?;
        fs::rename(&path, self.blob_path(hex))
    }

    /// Delete an upload; false if unknown.
    pub fn cancel_upload(&self, id: &str) -> io::Result<bool> {
        self.hashers().remove(id);
        remove(&self.upload_path(id))
    }

    // ---- manifests and tags

    pub fn has_manifest(&self, repo: &str, digest: &str) -> bool {
        self.marker(repo, MANIFESTS, digest)
            .is_some_and(|p| p.is_file())
    }

    /// Store manifest bytes and link them into `repo`, tagging if `tag` is set.
    /// Returns the digest.
    pub fn put_manifest(
        &self,
        repo: &str,
        bytes: &[u8],
        content_type: &str,
        tag: Option<&str>,
    ) -> io::Result<String> {
        let digest = digest::of(bytes);
        let hex = &digest[digest::PREFIX.len()..];
        if !self.blob_path(hex).is_file() {
            write_atomic(&self.blob_path(hex), bytes)?;
        }
        let dir = self.repo_dir(repo).join(MANIFESTS);
        // Type first, so a visible marker always has its type.
        write_atomic(&dir.join(format!("{hex}.type")), content_type.as_bytes())?;
        write_atomic(&dir.join(hex), b"")?;
        if let Some(tag) = tag {
            write_atomic(&self.repo_dir(repo).join(TAGS).join(tag), digest.as_bytes())?;
        }
        Ok(digest)
    }

    /// The digest a tag points at, if the tag exists.
    pub fn tag_digest(&self, repo: &str, tag: &str) -> io::Result<Option<String>> {
        let path = self.repo_dir(repo).join(TAGS).join(tag);
        Ok(found(fs::read_to_string(path))?.map(|d| d.trim().to_string()))
    }

    /// A manifest by digest, if `repo` holds it.
    pub fn manifest(&self, repo: &str, digest: &str) -> io::Result<Option<Manifest>> {
        if !self.has_manifest(repo, digest) {
            return Ok(None);
        }
        let hex = &digest[digest::PREFIX.len()..];
        let dir = self.repo_dir(repo).join(MANIFESTS);
        let content_type = found(fs::read_to_string(dir.join(format!("{hex}.type"))))?;
        let bytes = found(fs::read(self.blob_path(hex)))?;
        Ok(content_type
            .zip(bytes)
            .map(|(content_type, bytes)| Manifest {
                digest: digest.to_string(),
                content_type,
                bytes,
            }))
    }

    /// Digests of all manifests in `repo`, sorted.
    pub fn manifest_digests(&self, repo: &str) -> io::Result<Vec<String>> {
        let mut out: Vec<String> = names(&self.repo_dir(repo).join(MANIFESTS))?
            .into_iter()
            .filter(|n| is_hex64(n))
            .map(|n| format!("{}{n}", digest::PREFIX))
            .collect();
        out.sort();
        Ok(out)
    }

    /// Remove a manifest from `repo` and every tag pointing at it; false if unknown.
    pub fn delete_manifest(&self, repo: &str, digest: &str) -> io::Result<bool> {
        let Some(marker) = self.marker(repo, MANIFESTS, digest) else {
            return Ok(false);
        };
        if !remove(&marker)? {
            return Ok(false);
        }
        let hex = &digest[digest::PREFIX.len()..];
        remove(&marker.with_file_name(format!("{hex}.type")))?;
        for tag in self.tags(repo)? {
            if self.tag_digest(repo, &tag)?.as_deref() == Some(digest) {
                self.delete_tag(repo, &tag)?;
            }
        }
        Ok(true)
    }

    /// Remove a tag; false if unknown.
    pub fn delete_tag(&self, repo: &str, tag: &str) -> io::Result<bool> {
        remove(&self.repo_dir(repo).join(TAGS).join(tag))
    }

    /// Tags of `repo`, sorted (temp files skipped).
    pub fn tags(&self, repo: &str) -> io::Result<Vec<String>> {
        let mut out: Vec<String> = names(&self.repo_dir(repo).join(TAGS))?
            .into_iter()
            .filter(|n| !n.starts_with('.'))
            .collect();
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        (dir, s)
    }

    fn upload(s: &Store, bytes: &[u8]) -> String {
        let id = s.new_upload().unwrap();
        s.append_upload(&id, &mut &bytes[..]).unwrap();
        let (d, _) = s.upload_digest(&id).unwrap().unwrap();
        s.commit_upload(&id, &d).unwrap();
        d
    }

    #[test]
    fn ids() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
        assert!(is_upload_id(&a));
        assert!(!is_upload_id("../x"));
        assert!(!is_upload_id(&a.to_uppercase()));
        assert!(!is_upload_id(&a[1..]));
        assert!(is_hex64(&"a".repeat(64)));
        assert!(!is_hex64("a.type"));
    }

    #[test]
    fn write_atomic_replaces_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a/b/file");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        // Renaming onto a non-empty directory fails; no temp file is left behind.
        let d = dir.path().join("a/dir");
        fs::create_dir_all(d.join("x")).unwrap();
        assert!(write_atomic(&d, b"x").is_err());
        let left: Vec<_> = names(&dir.path().join("a")).unwrap();
        assert!(left.iter().all(|n| !n.starts_with(".tmp")), "{left:?}");
    }

    #[test]
    fn helpers_report_other_errors() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("file");
        fs::write(&f, b"x").unwrap();
        // A path through a regular file is ENOTDIR, not NotFound.
        assert!(names(&f.join("sub")).is_err());
        assert!(remove(dir.path()).is_err());
        assert!(names(&dir.path().join("missing")).unwrap().is_empty());
        assert!(!remove(&dir.path().join("missing")).unwrap());
    }

    #[test]
    fn blob_upload_link_and_open() {
        let (_d, s) = store();
        let d = upload(&s, b"hello");
        assert_eq!(d, digest::of(b"hello"));
        assert!(s.has_blob(&d));
        assert!(!s.has_blob("sha256:bad"));
        assert!(!s.has_blob_link("r", &d));
        assert!(!s.repo_exists("r").unwrap());
        s.link_blob("r", &d).unwrap();
        assert!(s.has_blob_link("r", &d));
        assert!(s.repo_exists("r").unwrap());
        let (mut f, n) = s.open_blob("r", &d).unwrap().unwrap();
        let mut got = String::new();
        f.read_to_string(&mut got).unwrap();
        assert_eq!((got.as_str(), n), ("hello", 5));
        assert!(s.open_blob("other", &d).unwrap().is_none());
        assert!(s.link_blob("r", "bad").is_err());
        assert!(!s.unlink_blob("r", "bad").unwrap());
        assert!(s.unlink_blob("r", &d).unwrap());
        assert!(!s.unlink_blob("r", &d).unwrap());
        assert!(!s.repo_exists("r").unwrap());
        // Uploading the same bytes again drops the duplicate.
        assert_eq!(upload(&s, b"hello"), d);
        assert!(names(&s.root.join("uploads")).unwrap().is_empty());
    }

    #[test]
    fn open_blob_with_missing_or_broken_file() {
        let (_d, s) = store();
        let d = upload(&s, b"x");
        s.link_blob("r", &d).unwrap();
        let hex = digest::hex_part(&d).unwrap();
        // Unreadable blob file: an error, not "unknown".
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(s.blob_path(hex), fs::Permissions::from_mode(0o000)).unwrap();
        assert!(s.open_blob("r", &d).is_err());
        // Blob bytes gone but the link left: unknown.
        fs::remove_file(s.blob_path(hex)).unwrap();
        assert!(s.open_blob("r", &d).unwrap().is_none());
    }

    #[test]
    fn chunked_upload_and_rehash_after_restart() {
        let (dir, s) = store();
        let id = s.new_upload().unwrap();
        assert_eq!(s.upload_len(&id).unwrap(), Some(0));
        assert_eq!(s.append_upload(&id, &mut &b"hel"[..]).unwrap(), Some(3));
        // A second Store (as after a restart) has no cached hasher.
        let s2 = Store::open(dir.path()).unwrap();
        assert_eq!(s2.append_upload(&id, &mut &b"lo"[..]).unwrap(), Some(5));
        assert_eq!(
            s2.upload_digest(&id).unwrap(),
            Some((digest::of(b"hello"), 5))
        );
        // The first store's cached hasher is stale (len 3 != 5) and is rebuilt.
        assert_eq!(
            s.upload_digest(&id).unwrap(),
            Some((digest::of(b"hello"), 5))
        );
        assert!(s.cancel_upload(&id).unwrap());
        assert!(!s.cancel_upload(&id).unwrap());
        assert_eq!(s.upload_len(&id).unwrap(), None);
        assert_eq!(s.upload_digest(&id).unwrap(), None);
        assert_eq!(s.append_upload(&id, &mut &b"x"[..]).unwrap(), None);
        assert!(s.commit_upload(&id, "bad").is_err());
        assert!(s.commit_upload(&id, &digest::of(b"zz")).is_err());
    }

    struct FailAfter(usize);
    impl Read for FailAfter {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0 == 0 {
                return Err(io::Error::other("boom"));
            }
            let n = self.0.min(buf.len());
            buf[..n].fill(b'a');
            self.0 -= n;
            Ok(n)
        }
    }

    #[test]
    fn append_keeps_partial_data_on_read_error() {
        let (_d, s) = store();
        let id = s.new_upload().unwrap();
        assert!(s.append_upload(&id, &mut FailAfter(4)).is_err());
        assert_eq!(s.upload_len(&id).unwrap(), Some(4));
        assert_eq!(
            s.upload_digest(&id).unwrap(),
            Some((digest::of(b"aaaa"), 4))
        );
    }

    #[test]
    fn upload_paths_that_are_directories_error() {
        let (_d, s) = store();
        let id = new_id();
        fs::create_dir_all(s.upload_path(&id).join("x")).unwrap();
        assert!(s.append_upload(&id, &mut &b""[..]).is_err());
        assert!(s.hasher_for(&id, 1).is_err());
        assert!(s.cancel_upload(&id).is_err());
        // upload_len of a path under a regular file: ENOTDIR.
        let f = new_id();
        fs::write(s.upload_path(&f), b"").unwrap();
        assert!(s.upload_len(&format!("{f}/x")).is_err());
        assert!(s.upload_digest(&format!("{f}/x")).is_err());
    }

    const OCI: &str = "application/vnd.oci.image.manifest.v1+json";

    #[test]
    fn manifests_and_tags() {
        let (_d, s) = store();
        let a = s
            .put_manifest("x/y", b"{\"a\":1}", OCI, Some("v1"))
            .unwrap();
        assert_eq!(a, digest::of(b"{\"a\":1}"));
        s.put_manifest("x/y", b"{\"a\":1}", OCI, Some("v2"))
            .unwrap();
        let b = s
            .put_manifest("x/y", b"{\"b\":1}", "t", Some("other"))
            .unwrap();
        assert!(s.has_manifest("x/y", &a));
        assert!(!s.has_manifest("x/y", "nope"));
        assert_eq!(s.tags("x/y").unwrap(), ["other", "v1", "v2"]);
        assert_eq!(s.tag_digest("x/y", "v1").unwrap(), Some(a.clone()));
        assert_eq!(s.tag_digest("x/y", "nope").unwrap(), None);
        let m = s.manifest("x/y", &a).unwrap().unwrap();
        assert_eq!(m.content_type, OCI);
        assert_eq!(m.bytes, b"{\"a\":1}");
        assert_eq!(m.digest, a);
        assert_eq!(s.manifest("other", &a).unwrap(), None);
        let mut both = vec![a.clone(), b.clone()];
        both.sort();
        assert_eq!(s.manifest_digests("x/y").unwrap(), both);
        assert_eq!(s.repos().unwrap(), ["x/y"]);

        assert!(s.delete_tag("x/y", "v2").unwrap());
        assert!(!s.delete_tag("x/y", "v2").unwrap());
        s.put_manifest("x/y", b"{\"a\":1}", OCI, Some("v2"))
            .unwrap();
        assert!(s.delete_manifest("x/y", &a).unwrap());
        assert!(!s.delete_manifest("x/y", &a).unwrap());
        assert!(!s.delete_manifest("x/y", "bad").unwrap());
        assert_eq!(s.tags("x/y").unwrap(), ["other"]);
        assert_eq!(s.manifest("x/y", &a).unwrap(), None);
        // Bytes stay in blobs/ for other repos.
        assert!(s.has_blob(&a));
    }

    #[test]
    fn manifest_with_missing_parts() {
        let (_d, s) = store();
        let a = s.put_manifest("r", b"{}", OCI, None).unwrap();
        let hex = &a[7..];
        let dir = s.repo_dir("r").join(MANIFESTS);
        fs::remove_file(dir.join(format!("{hex}.type"))).unwrap();
        assert_eq!(s.manifest("r", &a).unwrap(), None);
        fs::create_dir(dir.join(format!("{hex}.type"))).unwrap();
        assert!(s.manifest("r", &a).is_err());
        fs::remove_dir(dir.join(format!("{hex}.type"))).unwrap();
        fs::write(dir.join(format!("{hex}.type")), OCI).unwrap();
        fs::remove_file(s.blob_path(hex)).unwrap();
        assert_eq!(s.manifest("r", &a).unwrap(), None);
        fs::create_dir(s.blob_path(hex)).unwrap();
        assert!(s.manifest("r", &a).is_err());
        // Tag file that is a directory.
        fs::create_dir_all(s.repo_dir("r").join(TAGS).join("t")).unwrap();
        assert!(s.tag_digest("r", "t").is_err());
    }

    #[test]
    fn uses_cached_hash_while_length_matches() {
        // Same-length bytes swapped behind the store's back: the running hash
        // (not a re-read) decides the digest, so it still reports the original.
        let (_d, s) = store();
        let id = s.new_upload().unwrap();
        s.append_upload(&id, &mut &b"hello"[..]).unwrap();
        fs::write(s.upload_path(&id), b"HELLO").unwrap();
        assert_eq!(
            s.upload_digest(&id).unwrap(),
            Some((digest::of(b"hello"), 5))
        );
    }

    #[test]
    fn duplicate_upload_keeps_existing_blob_file() {
        use std::os::unix::fs::MetadataExt;
        let (_d, s) = store();
        let d = upload(&s, b"same");
        let hex = digest::hex_part(&d).unwrap().to_string();
        let ino = fs::metadata(s.blob_path(&hex)).unwrap().ino();
        assert_eq!(upload(&s, b"same"), d);
        assert_eq!(fs::metadata(s.blob_path(&hex)).unwrap().ino(), ino);
        // Same for manifest bytes that are already stored.
        s.put_manifest("r", b"same", "t", None).unwrap();
        assert_eq!(fs::metadata(s.blob_path(&hex)).unwrap().ino(), ino);
    }

    #[test]
    fn put_manifest_reuses_existing_blob() {
        let (_d, s) = store();
        let d = upload(&s, b"{}");
        assert_eq!(s.put_manifest("r", b"{}", OCI, None).unwrap(), d);
    }

    #[test]
    fn repos_walk_nested_and_skip_internal_dirs() {
        let (_d, s) = store();
        s.put_manifest("a", b"{}", OCI, None).unwrap();
        s.put_manifest("a/b/c", b"{}", OCI, None).unwrap();
        let d = upload(&s, b"z");
        s.link_blob("m", &d).unwrap();
        // Empty repo dir (everything deleted) does not count.
        s.put_manifest("gone", b"{}", OCI, None).unwrap();
        s.delete_manifest("gone", &digest::of(b"{}")).unwrap();
        // Stray files are ignored.
        fs::write(s.root.join("repos/stray"), b"").unwrap();
        fs::create_dir_all(s.root.join("repos/.tmp-x")).unwrap();
        assert_eq!(s.repos().unwrap(), ["a", "a/b/c", "m"]);
    }

    #[test]
    fn repo_exists_errors_propagate() {
        let (_d, s) = store();
        fs::create_dir_all(s.repo_dir("r")).unwrap();
        fs::write(s.repo_dir("r").join(BLOBS), b"").unwrap();
        assert!(s.repo_exists("r").is_err());
        assert!(s.repos().is_err());
        fs::remove_file(s.repo_dir("r").join(BLOBS)).unwrap();
        fs::write(s.repo_dir("r").join(MANIFESTS), b"").unwrap();
        assert!(s.repo_exists("r").is_err());
        assert!(s.manifest_digests("r").is_err());
        fs::write(s.repo_dir("r").join(TAGS), b"").unwrap();
        assert!(s.tags("r").is_err());
    }

    #[test]
    fn open_fails_on_file_root() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        fs::write(&f, b"").unwrap();
        assert!(Store::open(&f).is_err());
    }
}
