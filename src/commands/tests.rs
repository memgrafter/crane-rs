//! Command tests against an in-process crane-rs registry (port 0, temp dir), plus
//! scripted fakes for registry behaviours ours never produces.

use std::path::PathBuf;

use super::*;
use crate::fixture::{Image, docker_tarball, oci_layout};
use crate::registry::{ServeConfig, Server};
use crate::testutil::{Res, serve};

pub(crate) struct TestReg {
    pub server: Option<Server>,
    pub addr: String,
    _dir: tempfile::TempDir,
}

impl Drop for TestReg {
    fn drop(&mut self) {
        if let Some(s) = self.server.take() {
            s.shutdown();
        }
    }
}

pub(crate) fn reg() -> TestReg {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(&ServeConfig {
        address: Some("127.0.0.1:0".into()),
        disk: Some(dir.path().join("data")),
        verbose: false,
    })
    .unwrap();
    let addr = server.addr().to_string();
    TestReg {
        server: Some(server),
        addr,
        _dir: dir,
    }
}

fn ctx(platform: Option<&str>) -> Ctx {
    Ctx::with_keychain(false, platform, false, Keychain::default()).unwrap()
}

fn out_of(f: impl FnOnce(&mut Vec<u8>) -> Result<()>) -> String {
    let mut v = Vec::new();
    f(&mut v).unwrap();
    String::from_utf8(v).unwrap()
}

struct Files {
    dir: tempfile::TempDir,
    a: Image,
    b: Image,
}

impl Files {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (Image::small(1), Image::new(2, 1, 32, "linux", "arm64"));
        std::fs::write(dir.path().join("a.tar"), docker_tarball(&[("x/a:1", &a)])).unwrap();
        oci_layout(&dir.path().join("one"), &[&b], false).unwrap();
        oci_layout(&dir.path().join("multi"), &[&a, &b], true).unwrap();
        oci_layout(&dir.path().join("two"), &[&a, &b], false).unwrap();
        Self { dir, a, b }
    }
    fn p(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

/// Push the standard fixtures; returns (single image ref, index ref).
fn seeded(r: &TestReg, f: &Files) -> (String, String) {
    let c = ctx(None);
    let img = format!("{}/x/a:1", r.addr);
    let idx = format!("{}/x/multi:1", r.addr);
    out_of(|o| push(&c, &f.p("a.tar"), &img, false, o));
    out_of(|o| push(&c, &f.p("multi"), &idx, false, o));
    (img, idx)
}

#[test]
fn push_variants_print_digest_refs() {
    let (r, f) = (reg(), Files::new());
    let c = ctx(None);
    let got = out_of(|o| push(&c, &f.p("a.tar"), &format!("{}/x/a:1", r.addr), false, o));
    let want_manifest = crate::types::docker_manifest_bytes(
        &f.a.config_descriptor(media::DOCKER_CONFIG),
        &f.a.layer_descriptors(media::DOCKER_LAYER),
    );
    assert_eq!(
        got,
        format!("{}/x/a@{}\n", r.addr, crate::digest::of(&want_manifest))
    );
    let one = out_of(|o| push(&c, &f.p("one"), &format!("{}/x/one:1", r.addr), false, o));
    assert!(one.ends_with(&format!("@{}\n", crate::digest::of(&f.b.oci_manifest()))));
    out_of(|o| {
        push(
            &c,
            &f.p("multi"),
            &format!("{}/x/multi:1", r.addr),
            false,
            o,
        )
    });
    out_of(|o| push(&c, &f.p("two"), &format!("{}/x/two:1", r.addr), true, o));
    // pushing again: every blob already there
    out_of(|o| push(&c, &f.p("a.tar"), &format!("{}/x/a:2", r.addr), false, o));
}

#[test]
fn push_errors() {
    let (r, f) = (reg(), Files::new());
    let c = ctx(None);
    let dst = format!("{}/x/e:1", r.addr);
    let mut sink = Vec::new();
    let e = |p: &str| {
        format!(
            "{:#}",
            push(&c, &f.p(p), &dst, false, &mut Vec::new()).unwrap_err()
        )
    };
    assert!(e("missing").contains("missing"));
    assert!(e("two").contains("2 entries, consider --index"));
    std::fs::create_dir(f.p("plain")).unwrap();
    assert!(e("plain").contains("as OCI layout"));
    std::fs::write(f.p("bad.tar"), b"not a tar").unwrap();
    assert!(e("bad.tar").contains("as tarball"));
    // a layout whose single entry is not an image or index
    let odd = f.p("odd");
    oci_layout(&odd, &[&f.a], false).unwrap();
    let idx = std::fs::read_to_string(odd.join("index.json")).unwrap();
    std::fs::write(
        odd.join("index.json"),
        idx.replace(media::OCI_MANIFEST, "application/x-other"),
    )
    .unwrap();
    assert!(e("odd").contains("non-image"));
    assert!(push(&c, &f.p("a.tar"), "BAD REF", false, &mut sink).is_err());
}

#[test]
fn read_commands() {
    let (r, f) = (reg(), Files::new());
    let (img, idx) = seeded(&r, &f);
    let c = ctx(None);
    let d = out_of(|o| digest(&c, &img, false, o));
    assert_eq!(
        out_of(|o| digest(&c, &img, true, o)),
        format!("{}/x/a@{d}", r.addr)
    );
    let m = out_of(|o| manifest(&c, &img, o));
    assert_eq!(crate::digest::of(m.as_bytes()), d.trim());
    assert_eq!(
        out_of(|o| config(&c, &img, o)).as_bytes(),
        f.a.config.as_slice()
    );
    let cfg_ref = format!("{}/x/a@{}", r.addr, crate::digest::of(&f.a.config));
    assert_eq!(
        out_of(|o| blob(&c, &cfg_ref, o)).as_bytes(),
        f.a.config.as_slice()
    );
    assert!(
        blob(&c, &img, &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("must be a digest")
    );
    // index: default platform is linux/amd64 = image a
    assert_eq!(
        out_of(|o| config(&c, &idx, o)).as_bytes(),
        f.a.config.as_slice()
    );
    let arm = ctx(Some("linux/arm64"));
    assert_eq!(
        out_of(|o| config(&arm, &idx, o)).as_bytes(),
        f.b.config.as_slice()
    );
    assert_eq!(
        out_of(|o| manifest(&arm, &idx, o)).as_bytes(),
        f.b.oci_manifest().as_slice()
    );
    assert_eq!(
        out_of(|o| digest(&arm, &idx, false, o)),
        format!("{}\n", crate::digest::of(&f.b.oci_manifest()))
    );
    assert!(config(&ctx(Some("linux/s390x")), &idx, &mut Vec::new()).is_err());
    assert!(manifest(&c, &format!("{}/x/a:nope", r.addr), &mut Vec::new()).is_err());
    assert!(digest(&c, &format!("{}/x/a:nope", r.addr), false, &mut Vec::new()).is_err());
}

#[test]
fn tags_lists_and_deletes() {
    let (r, f) = (reg(), Files::new());
    let (img, _) = seeded(&r, &f);
    let c = ctx(None);
    tag(&c, &img, "2").unwrap();
    tag(&c, &img, "sha256-abc").unwrap();
    let repo = format!("{}/x/a", r.addr);
    assert_eq!(
        out_of(|o| ls(&c, &repo, false, false, o)),
        "1\n2\nsha256-abc\n"
    );
    assert_eq!(out_of(|o| ls(&c, &repo, false, true, o)), "1\n2\n");
    assert_eq!(
        out_of(|o| ls(&c, &repo, true, true, o)),
        format!("{repo}:1\n{repo}:2\n")
    );
    assert_eq!(out_of(|o| catalog(&c, &r.addr, false, o)), "x/a\nx/multi\n");
    assert_eq!(
        out_of(|o| catalog(&c, &r.addr, true, o)),
        format!("{0}/x/a\n{0}/x/multi\n", r.addr)
    );
    assert!(tag(&c, &img, "bad tag").is_err());
    assert!(tag(&c, &format!("{repo}:missing"), "t").is_err());
    delete(&c, &format!("{repo}:2")).unwrap();
    assert_eq!(out_of(|o| ls(&c, &repo, false, true, o)), "1\n");
    assert!(delete(&c, &format!("{repo}:2")).is_err());
    assert!(ls(&c, "BAD", false, false, &mut Vec::new()).is_err());
    assert!(catalog(&c, "a/b", false, &mut Vec::new()).is_err());
}

#[test]
fn copies() {
    let (r, f) = (reg(), Files::new());
    let other = reg();
    let (img, idx) = seeded(&r, &f);
    let c = ctx(None);
    let d_img = out_of(|o| digest(&c, &img, false, o));
    let d_idx = out_of(|o| digest(&c, &idx, false, o));
    // same registry: mount
    copy(&c, &img, &format!("{}/y/a:1", r.addr)).unwrap();
    assert_eq!(
        out_of(|o| digest(&c, &format!("{}/y/a:1", r.addr), false, o)),
        d_img
    );
    // other registry: upload; whole index
    copy(&c, &idx, &format!("{}/z/multi:1", other.addr)).unwrap();
    assert_eq!(
        out_of(|o| digest(&c, &format!("{}/z/multi:1", other.addr), false, o)),
        d_idx
    );
    // one platform
    let arm = ctx(Some("linux/arm64"));
    copy(&arm, &idx, &format!("{}/z/arm:1", other.addr)).unwrap();
    assert_eq!(
        out_of(|o| digest(&c, &format!("{}/z/arm:1", other.addr), false, o)),
        format!("{}\n", crate::digest::of(&f.b.oci_manifest()))
    );
    assert!(
        copy(
            &c,
            &format!("{}/y/none:1", r.addr),
            &format!("{}/y/x:1", r.addr)
        )
        .is_err()
    );
    assert!(copy(&c, &img, "BAD").is_err());
    assert!(copy(&c, "BAD", &img).is_err());
}

#[test]
fn pulls() {
    let (r, f) = (reg(), Files::new());
    let (img, idx) = seeded(&r, &f);
    let c = ctx(None);
    let work = tempfile::tempdir().unwrap();
    let t1 = work.path().join("one.tar");
    pull(&c, std::slice::from_ref(&img), &t1, Format::Tarball, false).unwrap();
    let back = out_of(|o| push(&c, &t1, &format!("{}/back/a:1", r.addr), false, o));
    let d = out_of(|o| digest(&c, &img, false, o));
    assert!(back.ends_with(&format!("@{d}")));
    // by digest (tagged i-was-a-digest) and two refs in one tarball
    let by_digest = format!("{}/x/a@{}", r.addr, d.trim());
    pull(
        &c,
        &[by_digest, idx.clone()],
        &work.path().join("two.tar"),
        Format::Tarball,
        false,
    )
    .unwrap();
    let names =
        String::from_utf8_lossy(&std::fs::read(work.path().join("two.tar")).unwrap()).into_owned();
    assert!(names.contains(&format!("{}/x/a:{I_WAS_A_DIGEST}", r.addr)));
    // failure leaves no tarball or temp file behind
    let bad = work.path().join("bad.tar");
    assert!(
        pull(
            &c,
            &[format!("{}/x/none:1", r.addr)],
            &bad,
            Format::Tarball,
            false
        )
        .is_err()
    );
    assert!(!bad.exists() && !work.path().join("bad.tar.tmp").exists());
    assert!(pull(&c, &["BAD REF".into()], &bad, Format::Tarball, false).is_err());

    // OCI: whole index without --platform, child with it, annotations, appending
    let lay = work.path().join("lay");
    pull(&c, std::slice::from_ref(&idx), &lay, Format::Oci, false).unwrap();
    pull(
        &ctx(Some("linux/arm64")),
        std::slice::from_ref(&idx),
        &lay,
        Format::Oci,
        true,
    )
    .unwrap();
    pull(&c, std::slice::from_ref(&img), &lay, Format::Oci, false).unwrap();
    let li = Layout::open(&lay).unwrap().index().unwrap();
    assert_eq!(li.manifests.len(), 3);
    assert_eq!(
        li.manifests[0].digest,
        out_of(|o| digest(&c, &idx, false, o)).trim()
    );
    assert_eq!(
        li.manifests[1].annotations[REF_NAME_ANNOTATION],
        format!("{}/x/multi:1", r.addr)
    );
    // with --platform only the child image is stored at the top level, not the index
    assert_eq!(
        li.manifests[1].digest,
        crate::digest::of(&f.b.oci_manifest())
    );
    assert!(
        pull(
            &c,
            &[format!("{}/x/none:1", r.addr)],
            &lay,
            Format::Oci,
            false
        )
        .is_err()
    );
}

#[test]
fn behaviours_only_a_fake_registry_shows() {
    let a = Image::small(5);
    let man = a.oci_manifest();
    let man_d = crate::digest::of(&man);
    let idx = format!(
        r#"{{"schemaVersion":2,"mediaType":"{}","manifests":[{{"mediaType":"{}","size":{},"digest":"{man_d}","platform":{{"os":"linux","architecture":"amd64"}}}}]}}"#,
        media::OCI_INDEX,
        media::OCI_MANIFEST,
        man.len()
    );
    let (man2, idx2) = (man.clone(), idx.clone());
    let cfg_d = crate::digest::of(&a.config);
    let f = serve(move |r| match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/v2/") => Res::new(200),
        // HEAD unsupported: digest falls back to GET
        ("HEAD", _) => Res::new(405),
        ("GET", "/v2/i/manifests/1") => Res::new(200)
            .header("Content-Type", media::OCI_INDEX)
            .body(idx2.clone()),
        // child served without Content-Type: the descriptor's media type is used
        ("GET", p) if p.starts_with("/v2/i/manifests/sha256:") => Res::new(200).body(man2.clone()),
        ("GET", "/v2/art/manifests/1") => Res::new(200).body(r#"{"schemaVersion":2,"layers":[]}"#),
        ("GET", p) if p.starts_with("/v2/i/blobs/") => Res::new(200).body("wrong bytes"),
        _ => Res::new(404),
    });
    let c = ctx(None);
    let base = format!("{}/i:1", f.addr);
    assert_eq!(
        out_of(|o| digest(&c, &base, false, o)),
        format!("{}\n", crate::digest::of(idx.as_bytes()))
    );
    let (_, fetched, _) = c.resolve_image(&Reference::parse(&base).unwrap()).unwrap();
    assert_eq!(fetched.content_type, media::OCI_MANIFEST);
    // config blob with wrong bytes
    assert!(
        config(&c, &base, &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("mismatch")
    );
    assert!(
        blob(&c, &format!("{}/i@{cfg_d}", f.addr), &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("mismatch")
    );
    // manifest without config
    assert!(
        config(&c, &format!("{}/art:1", f.addr), &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("no config")
    );
}

#[test]
fn ctx_platforms() {
    assert!(ctx(Some("all")).platform.is_none());
    assert!(Ctx::with_keychain(false, Some("bad"), false, Keychain::default()).is_err());
    assert!(Ctx::new(true, None, true).is_ok());
}

#[test]
fn mount_fallback_and_transfer_errors() {
    let img = Image::small(8);
    let man = img.oci_manifest();
    let man_d = crate::digest::of(&man);
    let idx = format!(
        r#"{{"schemaVersion":2,"mediaType":"{}","manifests":[{{"mediaType":"{}","size":{},"digest":"{man_d}"}}]}}"#,
        media::OCI_INDEX,
        media::OCI_MANIFEST,
        man.len()
    );
    let blobs: Vec<(String, Vec<u8>)> = std::iter::once(img.config.clone())
        .chain(img.layers.iter().cloned())
        .map(|b| (crate::digest::of(&b), b))
        .collect();
    let f = serve(move |r| {
        let (m, p) = (r.method.as_str(), r.path.as_str());
        let repo = p
            .trim_start_matches("/v2/")
            .split('/')
            .next()
            .unwrap_or_default();
        match (m, p) {
            ("GET", "/v2/") => Res::new(200),
            ("GET", _) if p.ends_with("/manifests/idx") => Res::new(200)
                .header("Content-Type", media::OCI_INDEX)
                .body(idx.clone()),
            ("GET", _) if p.contains("/manifests/") => Res::new(200)
                .header("Content-Type", media::OCI_MANIFEST)
                .body(man.clone()),
            // "half" serves only the config blob, so layer downloads fail
            ("GET", _) if p.contains("/blobs/") && repo == "half" && p.ends_with(&blobs[0].0) => {
                Res::new(200).body(blobs[0].1.clone())
            }
            ("GET", _) if p.contains("/blobs/") && repo == "good" => {
                let d = p.rsplit('/').next().unwrap();
                blobs
                    .iter()
                    .find(|(k, _)| k == d)
                    .map_or(Res::new(404), |(_, b)| Res::new(200).body(b.clone()))
            }
            ("HEAD", _) if p.contains("/blobs/") => Res::new(404),
            ("POST", _) if p.contains("mount=") => {
                Res::new(202).header("Location", &format!("/v2/{repo}/blobs/uploads/u"))
            }
            ("POST", _) if repo == "rejects" => Res::new(500),
            ("PUT", _) if p.contains("/blobs/uploads/u?digest=") => Res::new(201),
            ("PUT", _) if p.contains("/manifests/") => Res::new(201),
            _ => Res::new(404),
        }
    });
    let c = ctx(None);
    let at = |s: &str| format!("{}/{s}", f.addr);
    // same registry, mount refused (202): falls back to an upload at the returned URL
    copy(&c, &at("good:1"), &at("dst:1")).unwrap();
    assert!(
        f.calls()
            .iter()
            .any(|c| c.starts_with("PUT /v2/dst/blobs/uploads/u?digest="))
    );
    // source blobs missing: copy fails during the upload
    assert!(copy(&c, &at("broken:1"), &at("dst:2")).is_err());
    // pull of an index whose child's blobs are missing fails inside the child transfer
    let work = tempfile::tempdir().unwrap();
    assert!(
        pull(
            &c,
            &[at("broken:idx")],
            &work.path().join("l"),
            Format::Oci,
            false
        )
        .is_err()
    );
    // tarball pull: config downloads, a layer does not; nothing is left behind
    let t = work.path().join("half.tar");
    assert!(pull(&c, &[at("half:1")], &t, Format::Tarball, false).is_err());
    assert!(!t.exists());
    // push to a registry that refuses uploads
    let files = Files::new();
    assert!(
        push(
            &c,
            &files.p("a.tar"),
            &at("rejects:1"),
            false,
            &mut Vec::new()
        )
        .is_err()
    );
}

#[test]
fn only_needed_requests_are_sent() {
    let img = Image::small(6);
    let man = img.oci_manifest();
    let man_d = crate::digest::of(&man);
    let blobs: Vec<(String, Vec<u8>)> = std::iter::once(img.config.clone())
        .chain(img.layers.iter().cloned())
        .map(|b| (crate::digest::of(&b), b))
        .collect();
    let (m2, d2) = (man.clone(), man_d.clone());
    let src = serve(move |r| {
        let p = r.path.as_str();
        match r.method.as_str() {
            "GET" if p == "/v2/" => Res::new(200),
            // digest must use HEAD only: a GET of the manifest by tag is never needed
            "HEAD" if p.contains("/manifests/") => Res::new(200)
                .header("Docker-Content-Digest", &d2)
                .header("Content-Type", media::OCI_MANIFEST)
                .header("Content-Length", &m2.len().to_string()),
            "GET" if p.ends_with("/manifests/1") => Res::new(200)
                .header("Content-Type", media::OCI_MANIFEST)
                .body(m2.clone()),
            "GET" if p.contains("/blobs/") => {
                let d = p.rsplit('/').next().unwrap();
                blobs
                    .iter()
                    .find(|(k, _)| k == d)
                    .map_or(Res::new(404), |(_, b)| Res::new(200).body(b.clone()))
            }
            _ => Res::new(500),
        }
    });
    // destination: repo "full" already has every blob; others have none
    let dst = serve(|r| {
        let p = r.path.as_str();
        match r.method.as_str() {
            "GET" if p == "/v2/" => Res::new(200),
            "HEAD" if p.starts_with("/v2/full/blobs/") => Res::new(200),
            "HEAD" => Res::new(404),
            "POST" => Res::new(202).header("Location", "/v2/x/blobs/uploads/u"),
            "PUT" => Res::new(201),
            _ => Res::new(500),
        }
    });
    let c = ctx(None);
    assert_eq!(
        out_of(|o| digest(&c, &format!("{}/s:1", src.addr), false, o)),
        format!("{man_d}\n")
    );
    assert!(
        !src.calls()
            .iter()
            .any(|c| c.starts_with("GET /v2/s/manifests/")),
        "{:?}",
        src.calls()
    );
    // blobs already at the destination are not uploaded
    copy(
        &c,
        &format!("{}/s:1", src.addr),
        &format!("{}/full:1", dst.addr),
    )
    .unwrap();
    assert!(
        !dst.calls().iter().any(|c| c.starts_with("POST")),
        "{:?}",
        dst.calls()
    );
    // across registries there is no mount attempt
    copy(
        &c,
        &format!("{}/s:1", src.addr),
        &format!("{}/empty:1", dst.addr),
    )
    .unwrap();
    let calls = dst.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("POST /v2/empty/blobs/uploads/"))
    );
    assert!(!calls.iter().any(|c| c.contains("mount=")), "{calls:?}");
}
