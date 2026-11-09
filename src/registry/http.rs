//! Server-side HTTP/1.1 over any `BufRead` / `Write`: request heads, bodies
//! (Content-Length or chunked, streamed), and responses (bytes or a file range).
//! Malformed input is reported as `io::ErrorKind::InvalidData`; the caller
//! answers 400 and closes.

use std::fs::File;
use std::io::{self, BufRead, Read, Write};

/// Longest request line or header line.
const MAX_LINE: usize = 8 * 1024;
/// Most header lines in one request.
const MAX_HEADERS: usize = 100;
/// Longest chunk-size line or trailer line.
const MAX_CHUNK_LINE: usize = 1024;

pub const API_VERSION: (&str, &str) = ("Docker-Distribution-API-Version", "registry/2.0");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Framing {
    None,
    Length(u64),
    Chunked,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// Percent-decoded path, without the query.
    pub path: String,
    /// Decoded query pairs, in order.
    pub query: Vec<(String, String)>,
    /// HTTP/1.1 (keep-alive by default) rather than HTTP/1.0.
    pub http11: bool,
    /// Header names lower-cased, values trimmed.
    pub headers: Vec<(String, String)>,
    pub framing: Framing,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn query(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Whether the connection should close after this request.
    pub fn wants_close(&self) -> bool {
        let conn = self.header("connection").unwrap_or("").to_ascii_lowercase();
        if self.http11 {
            conn.split(',').any(|t| t.trim() == "close")
        } else {
            !conn.split(',').any(|t| t.trim() == "keep-alive")
        }
    }

    pub fn expects_continue(&self) -> bool {
        self.header("expect")
            .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
    }
}

pub fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Read one line ending in "\n" (at most `limit` bytes), without the line ending.
/// `Ok(None)` on EOF before any byte.
fn read_line(r: &mut dyn BufRead, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed mid-line",
            ));
        }
        let (take, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        line.extend_from_slice(&buf[..take]);
        r.consume(take);
        if line.len() > limit {
            return Err(invalid("line too long"));
        }
        if done {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
}

/// Read a request head. `Ok(None)` when the peer closed before sending anything.
pub fn read_head(r: &mut dyn BufRead) -> io::Result<Option<Request>> {
    // Reject non-HTTP (e.g. a TLS ClientHello) on the first byte, before waiting for a line.
    match r.fill_buf()?.first() {
        None => return Ok(None),
        Some(b) if !b.is_ascii_uppercase() => return Err(invalid("not an HTTP request")),
        Some(_) => {}
    }
    // At least one byte is buffered, so a line (possibly cut short: an error) follows.
    let line = read_line(r, MAX_LINE)?.unwrap_or_default();
    let line = String::from_utf8(line).map_err(|_| invalid("request line is not UTF-8"))?;
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid("malformed request line"));
    };
    if !method.bytes().all(|b| b.is_ascii_uppercase()) || !target.starts_with('/') {
        return Err(invalid("malformed request line"));
    }
    let http11 = match version {
        "HTTP/1.1" => true,
        "HTTP/1.0" => false,
        _ => return Err(invalid("unsupported HTTP version")),
    };
    let (raw_path, raw_query) = target.split_once('?').unwrap_or((target, ""));

    let mut headers = Vec::new();
    loop {
        let line =
            read_line(r, MAX_LINE)?.ok_or_else(|| invalid("connection closed in headers"))?;
        if line.is_empty() {
            break;
        }
        if headers.len() == MAX_HEADERS {
            return Err(invalid("too many headers"));
        }
        let line = String::from_utf8_lossy(&line);
        let Some((name, value)) = line.split_once(':') else {
            return Err(invalid("malformed header"));
        };
        if name.is_empty() || name.starts_with([' ', '\t']) || name.ends_with([' ', '\t']) {
            return Err(invalid("malformed header"));
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
    }

    let mut req = Request {
        method: method.to_string(),
        path: percent_decode(raw_path, false),
        query: parse_query(raw_query),
        http11,
        headers,
        framing: Framing::None,
    };
    req.framing = framing(&req)?;
    Ok(Some(req))
}

fn framing(req: &Request) -> io::Result<Framing> {
    if let Some(te) = req.header("transfer-encoding") {
        if te.eq_ignore_ascii_case("chunked") {
            return Ok(Framing::Chunked);
        }
        return Err(invalid("unsupported Transfer-Encoding"));
    }
    match req.header("content-length") {
        None => Ok(Framing::None),
        Some(v) => v
            .parse()
            .map(Framing::Length)
            .map_err(|_| invalid("bad Content-Length")),
    }
}

fn hex_val(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// Decode %XX escapes (and '+' as space when `plus`). Bad escapes are kept as is.
pub fn percent_decode(s: &str, plus: bool) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some((hi, lo)) = hex_val(b[i + 1]).zip(hex_val(b[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
            continue;
        }
        out.push(if plus && b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k, true), percent_decode(v, true))
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyState {
    /// Bytes left in a Content-Length body.
    Length(u64),
    /// Expecting a chunk-size line.
    ChunkStart,
    /// Bytes left in the current chunk.
    Chunk(u64),
    Done,
    /// A read failed; the body is unusable.
    Failed,
}

/// A request body, streamed from the connection.
pub struct Body<'a> {
    inner: &'a mut dyn BufRead,
    state: BodyState,
}

impl<'a> Body<'a> {
    pub fn new(inner: &'a mut dyn BufRead, framing: &Framing) -> Self {
        let state = match framing {
            Framing::None | Framing::Length(0) => BodyState::Done,
            Framing::Length(n) => BodyState::Length(*n),
            Framing::Chunked => BodyState::ChunkStart,
        };
        Body { inner, state }
    }

    pub fn is_done(&self) -> bool {
        self.state == BodyState::Done
    }

    pub fn failed(&self) -> bool {
        self.state == BodyState::Failed
    }

    /// Read and discard up to `limit` remaining bytes. True if the body ended.
    pub fn drain(&mut self, limit: u64) -> bool {
        let _ = io::copy(&mut self.by_ref().take(limit), &mut io::sink());
        self.is_done()
    }

    fn step(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.state {
                BodyState::Done | BodyState::Failed => return Ok(0),
                BodyState::Length(left) => {
                    let n = read_some(self.inner, buf, left)?;
                    self.state = match left - n as u64 {
                        0 => BodyState::Done,
                        rest => BodyState::Length(rest),
                    };
                    return Ok(n);
                }
                BodyState::ChunkStart => {
                    let line = read_line(self.inner, MAX_CHUNK_LINE)?
                        .ok_or_else(|| invalid("body ended before chunk size"))?;
                    let line = String::from_utf8_lossy(&line);
                    let size = line.split(';').next().unwrap_or("").trim();
                    let size =
                        u64::from_str_radix(size, 16).map_err(|_| invalid("bad chunk size"))?;
                    if size == 0 {
                        // Trailer section: header lines until an empty line.
                        while !read_line(self.inner, MAX_CHUNK_LINE)?
                            .ok_or_else(|| invalid("body ended in trailers"))?
                            .is_empty()
                        {}
                        self.state = BodyState::Done;
                        return Ok(0);
                    }
                    self.state = BodyState::Chunk(size);
                }
                BodyState::Chunk(left) => {
                    let n = read_some(self.inner, buf, left)?;
                    if left == n as u64 {
                        let end = read_line(self.inner, MAX_CHUNK_LINE)?;
                        if end.is_none_or(|l| !l.is_empty()) {
                            return Err(invalid("chunk not followed by CRLF"));
                        }
                        self.state = BodyState::ChunkStart;
                    } else {
                        self.state = BodyState::Chunk(left - n as u64);
                    }
                    return Ok(n);
                }
            }
        }
    }
}

/// Read 1..=min(buf.len(), left) bytes; EOF is an error (the body was cut short).
fn read_some(r: &mut dyn BufRead, buf: &mut [u8], left: u64) -> io::Result<usize> {
    let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
    let n = r.read(&mut buf[..want])?;
    // `want` > 0: callers pass a non-empty buffer and a non-zero `left`.
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "body shorter than declared",
        ));
    }
    Ok(n)
}

impl Read for Body<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.step(buf)
            .inspect_err(|_| self.state = BodyState::Failed)
    }
}

/// What a response carries after its headers.
pub enum Payload {
    Empty,
    Bytes(Vec<u8>),
    /// `len` bytes from the file's current position.
    File(File, u64),
}

impl Payload {
    pub fn len(&self) -> u64 {
        match self {
            Payload::Empty => 0,
            Payload::Bytes(b) => b.len() as u64,
            Payload::File(_, n) => *n,
        }
    }
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub payload: Payload,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Response {
            status,
            headers: Vec::new(),
            payload: Payload::Empty,
        }
    }

    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    pub fn bytes(mut self, content_type: &str, body: Vec<u8>) -> Self {
        self.headers
            .push(("Content-Type", content_type.to_string()));
        self.payload = Payload::Bytes(body);
        self
    }

    #[cfg(test)]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Write status line, headers (plus API version, Content-Length, and
    /// `Connection: close` when `close`), then the payload unless `head_only`.
    pub fn write_to(self, w: &mut dyn Write, head_only: bool, close: bool) -> io::Result<()> {
        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status, reason(self.status));
        for (n, v) in &self.headers {
            head.push_str(&format!("{n}: {v}\r\n"));
        }
        head.push_str(&format!("{}: {}\r\n", API_VERSION.0, API_VERSION.1));
        head.push_str(&format!("Content-Length: {}\r\n", self.payload.len()));
        if close {
            head.push_str("Connection: close\r\n");
        }
        head.push_str("\r\n");
        w.write_all(head.as_bytes())?;
        if !head_only {
            match self.payload {
                Payload::Empty => {}
                Payload::Bytes(b) => w.write_all(&b)?,
                Payload::File(f, n) => {
                    let copied = io::copy(&mut f.take(n), w)?;
                    if copied != n {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "file shorter than expected",
                        ));
                    }
                }
            }
        }
        w.flush()
    }
}

pub fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        416 => "Range Not Satisfiable",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor, Seek, SeekFrom};

    fn head(raw: &str) -> io::Result<Option<Request>> {
        read_head(&mut Cursor::new(raw.as_bytes().to_vec()))
    }

    fn bad(raw: &[u8]) {
        let err = read_head(&mut Cursor::new(raw.to_vec())).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{raw:?}: {err}");
    }

    #[test]
    fn parses_request_head() {
        let r = head(
            "PUT /v2/a%2Fb/blobs/uploads/x?digest=sha256%3Aab&n=1&flag&&q=a+b HTTP/1.1\r\n\
             Host: x\r\nContent-Length: 5\r\nX-A:  v  \r\n\r\nhello",
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.method, "PUT");
        assert_eq!(r.path, "/v2/a/b/blobs/uploads/x");
        assert_eq!(r.query("digest"), Some("sha256:ab"));
        assert_eq!(r.query("n"), Some("1"));
        assert_eq!(r.query("flag"), Some(""));
        assert_eq!(r.query("q"), Some("a b"));
        assert_eq!(r.query("missing"), None);
        assert_eq!(r.header("x-a"), Some("v"));
        assert_eq!(r.header("nope"), None);
        assert_eq!(r.framing, Framing::Length(5));
        assert!(r.http11);
        assert!(!r.wants_close());
        assert!(!r.expects_continue());
    }

    #[test]
    fn connection_and_expect_headers() {
        let r = head("GET / HTTP/1.1\r\nConnection: keep-alive, Close\r\n\r\n")
            .unwrap()
            .unwrap();
        assert!(r.wants_close());
        let r = head("GET / HTTP/1.0\r\n\r\n").unwrap().unwrap();
        assert!(!r.http11);
        assert!(r.wants_close());
        let r = head("GET / HTTP/1.0\r\nConnection: Keep-Alive\r\n\r\n")
            .unwrap()
            .unwrap();
        assert!(!r.wants_close());
        let r =
            head("PUT / HTTP/1.1\r\nExpect: 100-Continue\r\nTransfer-Encoding: chunked\r\n\r\n")
                .unwrap()
                .unwrap();
        assert!(r.expects_continue());
        assert_eq!(r.framing, Framing::Chunked);
        let r = head("GET / HTTP/1.1\nHost: a\n\n").unwrap().unwrap();
        assert_eq!(r.header("host"), Some("a"));
    }

    #[test]
    fn eof_and_garbage() {
        assert!(head("").unwrap().is_none());
        // TLS ClientHello starts with 0x16 0x03.
        bad(&[0x16, 0x03, 0x01, 0x02, 0x00]);
        bad(b"GET\r\n\r\n");
        bad(b"GET / HTTP/1.1 extra\r\n\r\n");
        bad(b"Get / HTTP/1.1\r\n\r\n");
        bad(b"GeT / HTTP/1.1\r\n\r\n");
        bad(b"GET x HTTP/1.1\r\n\r\n");
        bad(b"GET / HTTP/2\r\n\r\n");
        bad(b"GET /\xff HTTP/1.1\r\n\r\n");
        bad(b"GET / HTTP/1.1\r\nno colon\r\n\r\n");
        bad(b"GET / HTTP/1.1\r\n: empty name\r\n\r\n");
        bad(b"GET / HTTP/1.1\r\n folded: x\r\n\r\n");
        bad(b"GET / HTTP/1.1\r\nname : x\r\n\r\n");
        bad(b"GET / HTTP/1.1\r\nContent-Length: x\r\n\r\n");
        bad(b"GET / HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n");
        bad(format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(MAX_LINE)).as_bytes());
        bad("GET / HTTP/1.1\r\n".to_string().as_bytes()); // closed in headers
        let many = format!(
            "GET / HTTP/1.1\r\n{}\r\n",
            "a: b\r\n".repeat(MAX_HEADERS + 1)
        );
        bad(many.as_bytes());
        // EOF in the middle of the request line.
        let err = head("GET / HT").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn line_limits_are_exact() {
        // A request line of exactly MAX_LINE bytes (with CRLF) passes; one more fails.
        assert_eq!(MAX_LINE, 8192);
        let fixed = "GET / HTTP/1.1\r\n".len();
        let ok = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(MAX_LINE - fixed));
        assert_eq!(ok.find('\n').unwrap() + 1, MAX_LINE);
        assert!(head(&ok).unwrap().is_some());
        let over = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(MAX_LINE - fixed + 1));
        bad(over.as_bytes());
        // Exactly MAX_HEADERS headers pass.
        assert_eq!(MAX_HEADERS, 100);
        let full = format!("GET / HTTP/1.1\r\n{}\r\n", "a: b\r\n".repeat(MAX_HEADERS));
        assert_eq!(head(&full).unwrap().unwrap().headers.len(), MAX_HEADERS);
    }

    #[test]
    fn line_spanning_buffer_refills() {
        let raw = b"GET /abcdefghij HTTP/1.1\r\nHost: x\r\n\r\n";
        let mut r = BufReader::with_capacity(4, &raw[..]);
        let req = read_head(&mut r).unwrap().unwrap();
        assert_eq!(req.path, "/abcdefghij");
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%3a%3Ab", false), "a::b");
        assert_eq!(percent_decode("a+b", false), "a+b");
        assert_eq!(percent_decode("a+b", true), "a b");
        assert_eq!(percent_decode("%zz%4", false), "%zz%4");
        assert_eq!(percent_decode("%", false), "%");
        assert_eq!(percent_decode("%41", false), "A");
        assert_eq!(percent_decode("%ff", false), "\u{fffd}");
        assert_eq!(parse_query(""), vec![]);
    }

    fn read_body(raw: &[u8], framing: Framing) -> (io::Result<Vec<u8>>, bool, bool) {
        let mut cur = Cursor::new(raw.to_vec());
        let mut body = Body::new(&mut cur, &framing);
        let mut out = Vec::new();
        let res = body.read_to_end(&mut out).map(|_| out);
        (res, body.is_done(), body.failed())
    }

    #[test]
    fn length_bodies() {
        let (r, done, _) = read_body(b"hello world", Framing::Length(5));
        assert_eq!(r.unwrap(), b"hello");
        assert!(done);
        let (r, done, _) = read_body(b"", Framing::None);
        assert_eq!(r.unwrap(), b"");
        assert!(done);
        let (r, done, failed) = read_body(b"abc", Framing::Length(5));
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert!(!done && failed);
        // Zero-length reads are no-ops.
        let mut cur = Cursor::new(b"ab".to_vec());
        let mut body = Body::new(&mut cur, &Framing::Length(2));
        assert_eq!(body.read(&mut []).unwrap(), 0);
        assert!(!body.is_done());
    }

    #[test]
    fn chunked_bodies() {
        let raw = b"5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\nTrailer: x\r\n\r\nNEXT";
        let (r, done, _) = read_body(raw, Framing::Chunked);
        assert_eq!(r.unwrap(), b"hello world");
        assert!(done);
        // Small reads split a chunk.
        let mut cur = Cursor::new(raw.to_vec());
        let mut body = Body::new(&mut cur, &Framing::Chunked);
        let mut b = [0u8; 3];
        assert_eq!(body.read(&mut b).unwrap(), 3);
        assert_eq!(&b, b"hel");
        let mut rest = Vec::new();
        body.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"lo world");
        // Reads after the end return 0.
        assert_eq!(body.read(&mut b).unwrap(), 0);
        let mut tail = String::new();
        cur.read_to_string(&mut tail).unwrap();
        assert_eq!(tail, "NEXT");

        for bad in [
            &b"zz\r\nhello\r\n0\r\n\r\n"[..],
            b"5\r\nhelloXX0\r\n\r\n",
            b"5\r\nhello",
            b"5\r\nhel",
            b"",
            b"0\r\n",
        ] {
            let (r, done, failed) = read_body(bad, Framing::Chunked);
            assert!(r.is_err(), "{bad:?}");
            assert!(!done && failed);
        }
    }

    #[test]
    fn drain_respects_limit() {
        let mut cur = Cursor::new(b"0123456789".to_vec());
        let mut body = Body::new(&mut cur, &Framing::Length(10));
        assert!(!body.drain(4));
        assert!(body.drain(100));
    }

    #[test]
    fn writes_responses() {
        let mut out = Vec::new();
        Response::new(201)
            .header("Location", "/x")
            .bytes("application/json", b"{}".to_vec())
            .write_to(&mut out, false, true)
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "HTTP/1.1 201 Created\r\nLocation: /x\r\nContent-Type: application/json\r\n\
             Docker-Distribution-API-Version: registry/2.0\r\nContent-Length: 2\r\n\
             Connection: close\r\n\r\n{}"
        );

        let mut out = Vec::new();
        let r = Response::new(200).bytes("t", b"body".to_vec());
        assert_eq!(r.get("content-type"), Some("t"));
        assert_eq!(r.get("nope"), None);
        r.write_to(&mut out, true, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.ends_with("Content-Length: 4\r\n\r\n"), "{text}");

        let mut out = Vec::new();
        Response::new(204).write_to(&mut out, false, false).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .ends_with("Content-Length: 0\r\n\r\n")
        );
    }

    #[test]
    fn writes_file_payloads() {
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(b"0123456789").unwrap();
        f.seek(SeekFrom::Start(2)).unwrap();
        let mut r = Response::new(206);
        r.payload = Payload::File(f.try_clone().unwrap(), 3);
        let mut out = Vec::new();
        r.write_to(&mut out, false, false).unwrap();
        assert!(String::from_utf8(out).unwrap().ends_with("\r\n\r\n234"));

        // A file shorter than promised is an error.
        f.seek(SeekFrom::Start(8)).unwrap();
        let mut r = Response::new(200);
        r.payload = Payload::File(f, 5);
        let err = r.write_to(&mut Vec::new(), false, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn reason_phrases() {
        for (code, text) in [
            (100, "Continue"),
            (200, "OK"),
            (201, "Created"),
            (202, "Accepted"),
            (204, "No Content"),
            (206, "Partial Content"),
            (400, "Bad Request"),
            (404, "Not Found"),
            (405, "Method Not Allowed"),
            (413, "Payload Too Large"),
            (416, "Range Not Satisfiable"),
            (500, "Internal Server Error"),
            (299, "Unknown"),
        ] {
            assert_eq!(reason(code), text);
        }
    }
}
