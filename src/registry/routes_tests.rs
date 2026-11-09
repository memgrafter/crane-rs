//! Unit tests for routes.rs: requests built with `read_head`, handled in process.

use super::*;
use crate::registry::http::read_head;
use std::io::Cursor;

const OCI_MANIFEST: &str = media::OCI_MANIFEST;

struct T {
    dir: tempfile::TempDir,
    store: Store,
}

struct Got {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Got {
    fn h(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    fn code(&self) -> String {
        let e: ErrorResponse = serde_json::from_slice(&self.body).unwrap();
        e.errors[0].code.clone()
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

impl T {
    fn new() -> T {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        T { dir, store }
    }

    /// `head` is "METHOD target", then optional header lines after '\n'.
    fn req(&self, head: &str, body: &[u8]) -> Got {
        let mut raw = String::new();
        for (i, line) in head.split('\n').enumerate() {
            raw.push_str(line);
            raw.push_str(if i == 0 { " HTTP/1.1\r\n" } else { "\r\n" });
        }
        raw.push_str("\r\n");
        let req = read_head(&mut Cursor::new(raw.into_bytes()))
            .unwrap()
            .unwrap();
        let resp = handle(&self.store, &req, &mut &body[..]);
        let body = match resp.payload {
            Payload::Empty => Vec::new(),
            Payload::Bytes(b) => b,
            Payload::File(f, n) => {
                let mut v = Vec::new();
                f.take(n).read_to_end(&mut v).unwrap();
                v
            }
        };
        Got {
            status: resp.status,
            headers: resp.headers,
            body,
        }
    }

    fn push_blob(&self, repo: &str, bytes: &[u8]) -> String {
        let d = digest::of(bytes);
        let g = self.req(&format!("POST /v2/{repo}/blobs/uploads/?digest={d}"), bytes);
        assert_eq!(g.status, 201);
        d
    }

    fn put_manifest(&self, repo: &str, reference: &str, ct: &str, body: &str) -> Got {
        self.req(
            &format!("PUT /v2/{repo}/manifests/{reference}\nContent-Type: {ct}"),
            body.as_bytes(),
        )
    }

    /// Push config + one layer and an image manifest tagged `tag`; returns
    /// (manifest bytes, manifest digest).
    fn push_image(&self, repo: &str, tag: &str) -> (String, String) {
        let config = self.push_blob(repo, b"{\"os\":\"linux\"}");
        let layer = self.push_blob(repo, b"layer-bytes");
        let m = image_manifest(&config, &layer);
        let g = self.put_manifest(repo, tag, OCI_MANIFEST, &m);
        assert_eq!(g.status, 201, "{}", String::from_utf8_lossy(&g.body));
        let d = digest::of(m.as_bytes());
        (m, d)
    }
}

fn image_manifest(config: &str, layer: &str) -> String {
    format!(
        r#"{{"schemaVersion":2,"mediaType":"{OCI_MANIFEST}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","size":14,"digest":"{config}"}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","size":11,"digest":"{layer}"}}]}}"#
    )
}

fn s(x: &str) -> String {
    x.to_string()
}

#[test]
fn route_parsing() {
    use Route::*;
    let cases = [
        ("/v2", Some(Base)),
        ("/v2/", Some(Base)),
        ("/v2/_catalog", Some(Catalog)),
        ("/v2/a/b/tags/list", Some(Tags { name: s("a/b") })),
        ("/v2/a/blobs/uploads/", Some(UploadStart { name: s("a") })),
        ("/v2/a/blobs/uploads", Some(UploadStart { name: s("a") })),
        (
            "/v2/a/blobs/uploads/x",
            Some(Upload {
                name: s("a"),
                id: s("x"),
            }),
        ),
        (
            "/v2/a/blobs/b/blobs/d",
            Some(Blob {
                name: s("a/blobs/b"),
                digest: s("d"),
            }),
        ),
        (
            "/v2/a/manifests/x/manifests/t",
            Some(Manifest {
                name: s("a/manifests/x"),
                reference: s("t"),
            }),
        ),
        (
            "/v2/a/referrers/d",
            Some(Referrers {
                name: s("a"),
                digest: s("d"),
            }),
        ),
        ("/v2/tags/list", None),
        ("/v2/blobs/d", None),
        ("/v2/a/other/d", None),
        ("/v3/", None),
        ("/v2x", None),
        ("/", None),
    ];
    for (path, want) in cases {
        assert_eq!(parse_route(path), want, "{path}");
    }
}

#[test]
fn names_tags_digests() {
    let long = "x".repeat(255);
    for good in ["a", "a/b", "a0.b_c__d---e/f", long.as_str()] {
        assert!(is_name(good), "{good}");
    }
    let too_long = "x".repeat(256);
    for bad in [
        "", "A", "a/", "/a", "a//b", "-a", "a-", "a..b", "a___b", "a._b", "a/../b", "..", "a b",
        &too_long,
    ] {
        assert!(!is_name(bad), "{bad}");
    }
    let max_tag = "a".repeat(128);
    for good in ["latest", "_x", "V1.2-rc_3", &max_tag] {
        assert!(is_tag(good), "{good}");
    }
    let long_tag = "a".repeat(129);
    for bad in ["", ".x", "-x", "a/b", "a:b", &long_tag] {
        assert!(!is_tag(bad), "{bad}");
    }
    let d = digest::of(b"x");
    assert_eq!(parse_digest(&d).unwrap(), d);
    assert_eq!(parse_digest("sha512:abc").unwrap_err().code, "UNSUPPORTED");
    assert_eq!(
        parse_digest("sha256+b64:abc=").unwrap_err().code,
        "UNSUPPORTED"
    );
    for bad in [
        "sha256:abc",
        "abc",
        ":abc",
        "sha512:",
        "SHA512:abc",
        "sha512:a/b",
        "",
    ] {
        assert_eq!(
            parse_digest(bad).unwrap_err().code,
            "DIGEST_INVALID",
            "{bad}"
        );
    }
    assert_eq!(parse_reference("v1").unwrap(), Reference::Tag("v1".into()));
    assert_eq!(parse_reference(&d).unwrap(), Reference::Digest(d.clone()));
    assert_eq!(
        parse_reference("sha256:x").unwrap_err().code,
        "DIGEST_INVALID"
    );
    assert_eq!(parse_reference("-bad").unwrap_err().code, "TAG_INVALID");
}

#[test]
fn ranges() {
    assert_eq!(parse_range("bytes=0-4", 10), Some((0, 4)));
    assert_eq!(parse_range("bytes=5-", 10), Some((5, 9)));
    assert_eq!(parse_range("bytes=5-100", 10), Some((5, 9)));
    assert_eq!(parse_range("bytes=-3", 10), Some((7, 9)));
    assert_eq!(parse_range("bytes=-30", 10), Some((0, 9)));
    assert_eq!(parse_range("bytes=9-9", 10), Some((9, 9)));
    for bad in [
        "bytes=-0",
        "bytes=10-",
        "bytes=5-4",
        "bytes=x-1",
        "bytes=1-x",
        "bytes=-",
        "bytes=-x",
        "items=0-1",
        "bytes=0",
        "bytes=0-1,3-4",
    ] {
        assert_eq!(parse_range(bad, 10), None, "{bad}");
    }
    assert_eq!(parse_range("bytes=0-0", 0), None);

    assert_eq!(content_range_start("0-9"), Some(0));
    assert_eq!(content_range_start("bytes 5-9/10"), Some(5));
    assert_eq!(content_range_start("bytes=5-9"), Some(5));
    assert_eq!(content_range_start("9-5"), None);
    assert_eq!(content_range_start("5-5"), Some(5));
    assert_eq!(content_range_start("x-5"), None);
    assert_eq!(content_range_start("5-x"), None);
    assert_eq!(content_range_start("5"), None);
}

#[test]
fn base_and_unsupported() {
    let t = T::new();
    let g = t.req("GET /v2/", b"");
    assert_eq!((g.status, g.body.as_slice()), (200, &b"{}"[..]));
    assert_eq!(g.h("content-type"), Some(JSON));
    assert_eq!(t.req("HEAD /v2/", b"").status, 200);
    let d = digest::of(b"x");
    for head in [
        s("POST /v2/"),
        s("GET /nope"),
        s("DELETE /v2/_catalog"),
        format!("PATCH /v2/a/blobs/{d}"),
        s("GET /v2/a/blobs/uploads/"),
        s("POST /v2/a/blobs/uploads/0123456789abcdef0123456789abcdef"),
        s("POST /v2/a/manifests/latest"),
        s("PUT /v2/a/tags/list"),
        format!("PUT /v2/a/referrers/{d}"),
    ] {
        let g = t.req(&head, b"");
        assert_eq!((g.status, g.code()), (405, s("UNSUPPORTED")), "{head}");
    }
    let g = t.req("GET /v2/a/blobs/sha256:x", b"");
    assert_eq!((g.status, g.code()), (400, s("DIGEST_INVALID")));
}

#[test]
fn name_invalid_everywhere() {
    let t = T::new();
    let d = digest::of(b"x");
    for head in [
        format!("GET /v2/A/blobs/{d}"),
        s("POST /v2/a/../b/blobs/uploads/"),
        s("GET /v2/A/blobs/uploads/0123456789abcdef0123456789abcdef"),
        s("GET /v2/A/manifests/latest"),
        s("GET /v2/A/tags/list"),
        format!("GET /v2/A/referrers/{d}"),
    ] {
        let g = t.req(&head, b"");
        assert_eq!((g.status, g.code()), (400, s("NAME_INVALID")), "{head}");
    }
}

#[test]
fn monolithic_upload_and_blob_reads() {
    let t = T::new();
    let d = t.push_blob("a/b", b"hello world");
    let g = t.req(&format!("GET /v2/a/b/blobs/{d}"), b"");
    assert_eq!((g.status, g.body.as_slice()), (200, &b"hello world"[..]));
    assert_eq!(g.h("docker-content-digest"), Some(d.as_str()));
    assert_eq!(g.h("content-type"), Some("application/octet-stream"));

    let g = t.req(&format!("GET /v2/a/b/blobs/{d}\nRange: bytes=6-"), b"");
    assert_eq!((g.status, g.body.as_slice()), (206, &b"world"[..]));
    assert_eq!(g.h("content-range"), Some("bytes 6-10/11"));

    let g = t.req(&format!("GET /v2/a/b/blobs/{d}\nRange: bytes=11-"), b"");
    assert_eq!((g.status, g.code()), (416, s("BLOB_UNKNOWN")));
    assert_eq!(g.h("content-range"), Some("bytes */11"));

    let g = t.req(&format!("GET /v2/other/blobs/{d}"), b"");
    assert_eq!((g.status, g.code()), (404, s("BLOB_UNKNOWN")));

    let g = t.req(&format!("DELETE /v2/a/b/blobs/{d}"), b"");
    assert_eq!(g.status, 202);
    let g = t.req(&format!("DELETE /v2/a/b/blobs/{d}"), b"");
    assert_eq!((g.status, g.code()), (404, s("BLOB_UNKNOWN")));
    let g = t.req(&format!("HEAD /v2/a/b/blobs/{d}"), b"");
    assert_eq!(g.status, 404);

    // Wrong digest: rejected and nothing is kept.
    let wrong = digest::of(b"other");
    let g = t.req(
        &format!("POST /v2/a/blobs/uploads/?digest={wrong}"),
        b"hello",
    );
    assert_eq!((g.status, g.code()), (400, s("DIGEST_INVALID")));
    assert!(!t.store.has_blob(&wrong));
    let g = t.req("POST /v2/a/blobs/uploads/?digest=bad", b"hello");
    assert_eq!((g.status, g.code()), (400, s("DIGEST_INVALID")));
}

#[test]
fn chunked_upload_flow() {
    let t = T::new();
    let g = t.req("POST /v2/r/blobs/uploads/", b"");
    assert_eq!(g.status, 202);
    assert_eq!(g.h("range"), Some("0-0"));
    let loc = g.h("location").unwrap().to_string();
    let id = g.h("docker-upload-uuid").unwrap().to_string();
    assert_eq!(loc, format!("/v2/r/blobs/uploads/{id}"));

    let g = t.req(&format!("GET {loc}"), b"");
    assert_eq!((g.status, g.h("range")), (204, Some("0-0")));

    let g = t.req(&format!("PATCH {loc}"), b"hello ");
    assert_eq!((g.status, g.h("range")), (202, Some("0-5")));
    assert_eq!(g.h("location"), Some(loc.as_str()));

    // Out-of-order chunk.
    let g = t.req(&format!("PATCH {loc}\nContent-Range: 0-4"), b"world");
    assert_eq!((g.status, g.code()), (416, s("BLOB_UPLOAD_INVALID")));
    assert_eq!(g.h("range"), Some("0-5"));
    let g = t.req(&format!("PATCH {loc}\nContent-Range: junk"), b"world");
    assert_eq!(g.status, 416);

    let g = t.req(&format!("PATCH {loc}\nContent-Range: 6-10"), b"world");
    assert_eq!((g.status, g.h("range")), (202, Some("0-10")));
    let g = t.req(&format!("HEAD {loc}"), b"");
    assert_eq!((g.status, g.h("range")), (204, Some("0-10")));

    // PUT without a digest.
    let g = t.req(&format!("PUT {loc}"), b"");
    assert_eq!((g.status, g.code()), (400, s("DIGEST_INVALID")));

    let d = digest::of(b"hello world!");
    let g = t.req(&format!("PUT {loc}?digest={d}"), b"!");
    assert_eq!(g.status, 201);
    assert_eq!(g.h("location"), Some(format!("/v2/r/blobs/{d}").as_str()));
    assert_eq!(g.h("docker-content-digest"), Some(d.as_str()));
    assert!(t.store.has_blob_link("r", &d));

    // The upload is gone.
    for head in [
        format!("GET {loc}"),
        format!("PATCH {loc}"),
        format!("PUT {loc}?digest={d}"),
        format!("DELETE {loc}"),
    ] {
        let g = t.req(&head, b"");
        assert_eq!(
            (g.status, g.code()),
            (404, s("BLOB_UPLOAD_UNKNOWN")),
            "{head}"
        );
    }
    let g = t.req("GET /v2/r/blobs/uploads/not-an-id", b"");
    assert_eq!((g.status, g.code()), (404, s("BLOB_UPLOAD_UNKNOWN")));

    // A mismatched final digest discards the upload.
    let g = t.req("POST /v2/r/blobs/uploads/", b"");
    let loc = g.h("location").unwrap().to_string();
    let g = t.req(&format!("PUT {loc}?digest={d}"), b"nope");
    assert_eq!((g.status, g.code()), (400, s("DIGEST_INVALID")));
    assert_eq!(t.req(&format!("GET {loc}"), b"").status, 404);

    // Cancel.
    let g = t.req("POST /v2/r/blobs/uploads", b"");
    let loc = g.h("location").unwrap().to_string();
    assert_eq!(t.req(&format!("DELETE {loc}"), b"").status, 204);
    assert_eq!(t.req(&format!("GET {loc}"), b"").status, 404);
    assert_eq!(t.req(&format!("POST {loc}"), b"").status, 405);
}

#[test]
fn cross_repo_mount() {
    let t = T::new();
    let d = t.push_blob("src", b"shared");
    let g = t.req(
        &format!("POST /v2/dst/blobs/uploads/?mount={d}&from=src"),
        b"",
    );
    assert_eq!(g.status, 201);
    assert_eq!(g.h("location"), Some(format!("/v2/dst/blobs/{d}").as_str()));
    assert_eq!(g.h("docker-content-digest"), Some(d.as_str()));
    assert!(t.store.has_blob_link("dst", &d));

    // Source lacks the blob, or the source name is invalid: a normal upload starts.
    let other = digest::of(b"absent");
    for q in [
        format!("mount={other}&from=src"),
        format!("mount={d}&from=BAD"),
    ] {
        let g = t.req(&format!("POST /v2/dst2/blobs/uploads/?{q}"), b"");
        assert_eq!(g.status, 202, "{q}");
        assert!(g.h("docker-upload-uuid").is_some());
    }
    assert!(!t.store.has_blob_link("dst2", &d));
    let g = t.req("POST /v2/dst/blobs/uploads/?mount=bad&from=src", b"");
    assert_eq!((g.status, g.code()), (400, s("DIGEST_INVALID")));
    // mount without from: a normal upload.
    let g = t.req(&format!("POST /v2/dst3/blobs/uploads/?mount={d}"), b"");
    assert_eq!(g.status, 202);
    assert!(!t.store.has_blob_link("dst3", &d));
}

#[test]
fn manifests_put_get_delete() {
    let t = T::new();
    let (m, d) = t.push_image("img", "v1");
    for r in ["v1", d.as_str()] {
        let g = t.req(&format!("GET /v2/img/manifests/{r}"), b"");
        assert_eq!(g.status, 200);
        assert_eq!(g.body, m.as_bytes());
        assert_eq!(g.h("content-type"), Some(OCI_MANIFEST));
        assert_eq!(g.h("docker-content-digest"), Some(d.as_str()));
    }
    assert_eq!(t.req("HEAD /v2/img/manifests/v1", b"").status, 200);

    // PUT by digest: must match.
    let g = t.put_manifest("img", &d, OCI_MANIFEST, &m);
    assert_eq!(g.status, 201);
    assert_eq!(
        g.h("location"),
        Some(format!("/v2/img/manifests/{d}").as_str())
    );
    assert_eq!(g.h("docker-content-digest"), Some(d.as_str()));
    assert_eq!(g.h("oci-subject"), None);
    let wrong = digest::of(b"x");
    let g = t.put_manifest("img", &wrong, OCI_MANIFEST, &m);
    assert_eq!((g.status, g.code()), (400, s("DIGEST_INVALID")));

    // Unknown references.
    for r in ["nope", wrong.as_str()] {
        let g = t.req(&format!("GET /v2/img/manifests/{r}"), b"");
        assert_eq!((g.status, g.code()), (404, s("MANIFEST_UNKNOWN")), "{r}");
    }
    let g = t.req("GET /v2/img/manifests/bad:tag:", b"");
    assert_eq!(g.code(), "DIGEST_INVALID");
    let g = t.req("GET /v2/img/manifests/-x", b"");
    assert_eq!((g.status, g.code()), (400, s("TAG_INVALID")));

    // Tag delete removes only the tag.
    t.put_manifest("img", "v2", OCI_MANIFEST, &m);
    assert_eq!(t.req("DELETE /v2/img/manifests/v2", b"").status, 202);
    assert_eq!(t.req("GET /v2/img/manifests/v2", b"").status, 404);
    assert_eq!(t.req("GET /v2/img/manifests/v1", b"").status, 200);
    let g = t.req("DELETE /v2/img/manifests/v2", b"");
    assert_eq!((g.status, g.code()), (404, s("MANIFEST_UNKNOWN")));

    // Digest delete removes the manifest and its tags.
    let g = t.req(&format!("DELETE /v2/img/manifests/{d}"), b"");
    assert_eq!(g.status, 202);
    assert_eq!(t.req("GET /v2/img/manifests/v1", b"").status, 404);
    let g = t.req(&format!("DELETE /v2/img/manifests/{d}"), b"");
    assert_eq!((g.status, g.code()), (404, s("MANIFEST_UNKNOWN")));
}

#[test]
fn manifest_validation() {
    let t = T::new();
    let config = t.push_blob("r", b"{\"os\":\"linux\"}");
    let layer = t.push_blob("r", b"layer-bytes");
    let ok = image_manifest(&config, &layer);

    let bad = |ct: &str, body: &str| {
        let g = t.put_manifest("r", "t", ct, body);
        (g.status, g.code())
    };
    let invalid = (400, s("MANIFEST_INVALID"));
    assert_eq!(bad(OCI_MANIFEST, "not json"), invalid);
    assert_eq!(bad(OCI_MANIFEST, "[]"), invalid);
    assert_eq!(bad(OCI_MANIFEST, r#"{"layers":"x"}"#), invalid);
    // No Content-Type and no (or an empty) mediaType.
    let g = t.req("PUT /v2/r/manifests/t", br#"{"schemaVersion":2}"#);
    assert_eq!((g.status, g.code()), invalid);
    let g = t.req(
        "PUT /v2/r/manifests/t\nContent-Type: ",
        br#"{"mediaType":""}"#,
    );
    assert_eq!((g.status, g.code()), invalid);
    // mediaType in the body stands in for a missing Content-Type.
    let g = t.req("PUT /v2/r/manifests/t", ok.as_bytes());
    assert_eq!(g.status, 201);
    let g = t.req("GET /v2/r/manifests/t", b"");
    assert_eq!(g.h("content-type"), Some(OCI_MANIFEST));
    // An explicit Content-Type wins over mediaType.
    t.put_manifest("r", "t2", "application/x-custom", &ok);
    let g = t.req("GET /v2/r/manifests/t2", b"");
    assert_eq!(g.h("content-type"), Some("application/x-custom"));

    // Missing blobs.
    let absent = digest::of(b"absent");
    let blob_unknown = (400, s("MANIFEST_BLOB_UNKNOWN"));
    assert_eq!(
        bad(OCI_MANIFEST, &image_manifest(&absent, &layer)),
        blob_unknown
    );
    assert_eq!(
        bad(OCI_MANIFEST, &image_manifest(&config, &absent)),
        blob_unknown
    );
    // A blob present in another repo only.
    let elsewhere = t.push_blob("other", b"elsewhere");
    assert_eq!(
        bad(OCI_MANIFEST, &image_manifest(&config, &elsewhere)),
        blob_unknown
    );
    // Foreign layers (with urls) need not be present.
    let foreign = format!(
        r#"{{"schemaVersion":2,"config":{{"mediaType":"c","size":1,"digest":"{config}"}},"layers":[{{"mediaType":"l","size":1,"digest":"{absent}","urls":["http://x"]}}]}}"#
    );
    assert_eq!(t.put_manifest("r", "f", OCI_MANIFEST, &foreign).status, 201);

    // Size limit: exactly 4 MiB is fine, one byte more is not.
    let exactly = format!("{{\"x\":\"{}\"}}", "a".repeat(MAX_MANIFEST as usize - 8));
    assert_eq!(exactly.len() as u64, MAX_MANIFEST);
    assert_eq!(
        t.put_manifest("r", "max", "application/x", &exactly).status,
        201
    );
    let over = format!("{{\"x\":\"{}\"}}", "a".repeat(MAX_MANIFEST as usize - 7));
    assert_eq!(bad(OCI_MANIFEST, &over), (413, s("SIZE_INVALID")));
}

#[test]
fn index_manifests() {
    let t = T::new();
    let (m, d) = t.push_image("r", "img");
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"{}","manifests":[{{"mediaType":"{OCI_MANIFEST}","size":{},"digest":"{d}"}}]}}"#,
        media::OCI_INDEX,
        m.len()
    );
    let g = t.put_manifest("r", "idx", media::OCI_INDEX, &index);
    assert_eq!(g.status, 201);
    // The child must be in the same repo.
    let g = t.put_manifest("other", "idx", media::OCI_INDEX, &index);
    assert_eq!((g.status, g.code()), (400, s("MANIFEST_BLOB_UNKNOWN")));
    // An index body is checked as an index even under another Content-Type.
    let g = t.put_manifest("other", "idx", "application/x", &index);
    assert_eq!((g.status, g.code()), (400, s("MANIFEST_BLOB_UNKNOWN")));
    let g = t.put_manifest("other", "idx", media::DOCKER_LIST, r#"{"manifests":[]}"#);
    assert_eq!(g.status, 201);
}

#[test]
fn tags_and_catalog_paging() {
    let t = T::new();
    let g = t.req("GET /v2/r/tags/list", b"");
    assert_eq!((g.status, g.code()), (404, s("NAME_UNKNOWN")));
    // A repo with only a blob exists, with no tags.
    t.push_blob("blobonly", b"x");
    let g = t.req("GET /v2/blobonly/tags/list", b"");
    assert_eq!(g.json(), json!({"name":"blobonly","tags":[]}));

    let (m, _) = t.push_image("r", "c");
    for tag in ["a", "b", "d"] {
        t.put_manifest("r", tag, OCI_MANIFEST, &m);
    }
    let g = t.req("GET /v2/r/tags/list", b"");
    assert_eq!(g.json(), json!({"name":"r","tags":["a","b","c","d"]}));
    assert_eq!(g.h("link"), None);
    let g = t.req("GET /v2/r/tags/list?n=2", b"");
    assert_eq!(g.json()["tags"], json!(["a", "b"]));
    assert_eq!(
        g.h("link"),
        Some(r#"</v2/r/tags/list?n=2&last=b>; rel="next""#)
    );
    let g = t.req("GET /v2/r/tags/list?n=2&last=b", b"");
    assert_eq!(g.json()["tags"], json!(["c", "d"]));
    assert_eq!(g.h("link"), None);
    let g = t.req("GET /v2/r/tags/list?n=4", b"");
    assert_eq!(
        (g.json()["tags"].as_array().unwrap().len(), g.h("link")),
        (4, None)
    );
    let g = t.req("GET /v2/r/tags/list?n=0", b"");
    assert_eq!((g.json()["tags"].clone(), g.h("link")), (json!([]), None));
    let g = t.req("GET /v2/r/tags/list?last=c", b"");
    assert_eq!(g.json()["tags"], json!(["d"]));
    let g = t.req("GET /v2/r/tags/list?n=x", b"");
    assert_eq!((g.status, g.code()), (400, s("UNSUPPORTED")));

    t.push_blob("a/b", b"x");
    let g = t.req("GET /v2/_catalog", b"");
    assert_eq!(g.json(), json!({"repositories":["a/b","blobonly","r"]}));
    let g = t.req("GET /v2/_catalog?n=1&last=a/b", b"");
    assert_eq!(g.json(), json!({"repositories":["blobonly"]}));
    assert_eq!(
        g.h("link"),
        Some(r#"</v2/_catalog?n=1&last=blobonly>; rel="next""#)
    );
    assert_eq!(t.req("GET /v2/_catalog?n=-1", b"").status, 400);
}

#[test]
fn referrers_api() {
    let t = T::new();
    let (_, subject) = t.push_image("r", "base");
    let empty = t.push_blob("r", b"{}");
    let artifact = |atype: Option<&str>, ann: &str| {
        let at = atype
            .map(|a| format!(r#""artifactType":"{a}","#))
            .unwrap_or_default();
        format!(
            r#"{{"schemaVersion":2,"mediaType":"{OCI_MANIFEST}",{at}"config":{{"mediaType":"application/vnd.oci.empty.v1+json","size":2,"digest":"{empty}"}},"layers":[],"subject":{{"mediaType":"{OCI_MANIFEST}","size":1,"digest":"{subject}"}}{ann}}}"#
        )
    };
    let sig = artifact(
        Some("application/vnd.example.sig"),
        r#","annotations":{"k":"v"}"#,
    );
    let sbom = artifact(None, "");
    let sig_d = digest::of(sig.as_bytes());
    let sbom_d = digest::of(sbom.as_bytes());
    let g = t.put_manifest("r", &sig_d, OCI_MANIFEST, &sig);
    assert_eq!(g.status, 201);
    assert_eq!(g.h("oci-subject"), Some(subject.as_str()));
    assert_eq!(
        t.put_manifest("r", &sbom_d, OCI_MANIFEST, &sbom).status,
        201
    );
    // An index referring to the subject too, without artifactType.
    let idx = format!(
        r#"{{"schemaVersion":2,"mediaType":"{}","manifests":[],"subject":{{"mediaType":"{OCI_MANIFEST}","size":1,"digest":"{subject}"}}}}"#,
        media::OCI_INDEX
    );
    let idx_d = digest::of(idx.as_bytes());
    assert_eq!(
        t.put_manifest("r", "idx", media::OCI_INDEX, &idx).status,
        201
    );

    let g = t.req(&format!("GET /v2/r/referrers/{subject}"), b"");
    assert_eq!(g.status, 200);
    assert_eq!(g.h("content-type"), Some(media::OCI_INDEX));
    assert_eq!(g.h("oci-filters-applied"), None);
    let v = g.json();
    assert_eq!(v["schemaVersion"], 2);
    assert_eq!(v["mediaType"], media::OCI_INDEX);
    let mut want = vec![
        json!({"mediaType": OCI_MANIFEST, "digest": sig_d, "size": sig.len(),
               "artifactType": "application/vnd.example.sig", "annotations": {"k":"v"}}),
        json!({"mediaType": OCI_MANIFEST, "digest": sbom_d, "size": sbom.len(),
               "artifactType": "application/vnd.oci.empty.v1+json"}),
        json!({"mediaType": media::OCI_INDEX, "digest": idx_d, "size": idx.len()}),
    ];
    want.sort_by_key(|d| d["digest"].as_str().unwrap().to_string());
    assert_eq!(v["manifests"], json!(want));

    let g = t.req(
        &format!("GET /v2/r/referrers/{subject}?artifactType=application/vnd.example.sig"),
        b"",
    );
    assert_eq!(g.h("oci-filters-applied"), Some("artifactType"));
    assert_eq!(g.json()["manifests"].as_array().unwrap().len(), 1);
    assert_eq!(g.json()["manifests"][0]["digest"], sig_d.as_str());

    // Unknown subject or repo: an empty index.
    let g = t.req(&format!("GET /v2/nope/referrers/{subject}"), b"");
    assert_eq!((g.status, g.json()["manifests"].clone()), (200, json!([])));
    let g = t.req("GET /v2/r/referrers/sha256:bad", b"");
    assert_eq!(g.code(), "DIGEST_INVALID");

    // A manifest whose type file vanished is skipped.
    let hex = &sig_d[7..];
    let type_file = t.dir.path().join(format!("repos/r/_manifests/{hex}.type"));
    std::fs::remove_file(type_file).unwrap();
    let g = t.req(&format!("GET /v2/r/referrers/{subject}"), b"");
    assert_eq!(g.json()["manifests"].as_array().unwrap().len(), 2);
}

#[test]
fn storage_errors_are_500() {
    let t = T::new();
    t.push_image("r", "v1");
    // Make repos/r/_tags a file so listing tags fails.
    let tags = t.dir.path().join("repos/r/_tags");
    std::fs::remove_dir_all(&tags).unwrap();
    std::fs::write(&tags, b"").unwrap();
    let g = t.req("GET /v2/r/tags/list", b"");
    assert_eq!((g.status, g.code()), (500, s("UNKNOWN")));
}
