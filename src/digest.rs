//! sha256 digests: "sha256:" + 64 lower-case hex chars. Only sha256 is supported.

use std::io::{self, Read, Write};

use sha2::{Digest as _, Sha256};

pub const PREFIX: &str = "sha256:";

/// Lower-case hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// Digest of a byte slice.
pub fn of(bytes: &[u8]) -> String {
    format!("{PREFIX}{}", hex(&Sha256::digest(bytes)))
}

/// The hex part of a valid sha256 digest, or None.
pub fn hex_part(digest: &str) -> Option<&str> {
    let h = digest.strip_prefix(PREFIX)?;
    (h.len() == 64
        && h.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(h)
}

pub fn is_valid(digest: &str) -> bool {
    hex_part(digest).is_some()
}

/// Running sha256 + byte count.
#[derive(Default, Clone)]
pub struct Hasher {
    sha: Sha256,
    len: u64,
}

impl Hasher {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn update(&mut self, bytes: &[u8]) {
        self.sha.update(bytes);
        self.len += bytes.len() as u64;
    }
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// ("sha256:<hex>", byte count)
    pub fn finish(self) -> (String, u64) {
        (format!("{PREFIX}{}", hex(&self.sha.finalize())), self.len)
    }
}

/// Reader that hashes everything read through it.
pub struct HashingReader<R> {
    inner: R,
    hasher: Hasher,
}

impl<R: Read> HashingReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Hasher::new(),
        }
    }
    pub fn finish(self) -> (String, u64) {
        self.hasher.finish()
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

/// Writer that hashes everything written through it.
pub struct HashingWriter<W> {
    inner: W,
    hasher: Hasher,
}

impl<W: Write> HashingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Hasher::new(),
        }
    }
    /// (inner writer, "sha256:<hex>", byte count)
    pub fn finish(self) -> (W, String, u64) {
        let (d, n) = self.hasher.finish();
        (self.inner, d, n)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn of_empty_and_abc() {
        assert_eq!(of(b""), EMPTY);
        assert_eq!(
            of(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hex_encodes_all_nibbles() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    }

    #[test]
    fn validation() {
        assert!(is_valid(EMPTY));
        assert_eq!(hex_part(EMPTY), Some(&EMPTY[7..]));
        assert!(!is_valid("sha512:e3b0"));
        assert!(!is_valid(&EMPTY[..EMPTY.len() - 1]));
        assert!(!is_valid(&EMPTY.to_uppercase().replace("SHA256", "sha256")));
        assert!(!is_valid(&format!("{}g", &EMPTY[..EMPTY.len() - 1])));
        assert!(!is_valid(&format!("{EMPTY}0")));
        // every hex digit accepted
        assert!(is_valid(&format!(
            "{PREFIX}{}",
            "0123456789abcdef".repeat(4)
        )));
    }

    #[test]
    fn hashing_writer_flush_reaches_inner() {
        let mut w = HashingWriter::new(io::BufWriter::new(Vec::new()));
        w.write_all(b"abc").unwrap();
        w.flush().unwrap();
        let (inner, _, _) = w.finish();
        assert!(inner.buffer().is_empty());
        assert_eq!(inner.get_ref(), b"abc");
    }

    #[test]
    fn hasher_counts_and_hashes() {
        let mut h = Hasher::new();
        assert!(h.is_empty());
        h.update(b"ab");
        h.update(b"c");
        assert_eq!(h.len(), 3);
        assert!(!h.is_empty());
        assert_eq!(h.finish(), (of(b"abc"), 3));
    }

    #[test]
    fn hashing_reader_and_writer() {
        let mut r = HashingReader::new(&b"hello world"[..]);
        let mut sink = Vec::new();
        io::copy(&mut r, &mut sink).unwrap();
        assert_eq!(r.finish(), (of(b"hello world"), 11));

        let mut w = HashingWriter::new(Vec::new());
        w.write_all(b"hello ").unwrap();
        w.write_all(b"world").unwrap();
        w.flush().unwrap();
        let (inner, d, n) = w.finish();
        assert_eq!(inner, b"hello world");
        assert_eq!((d, n), (of(b"hello world"), 11));
    }
}
