use std::collections::HashSet;
use std::io::Read;

use super::*;
use crate::fixture::{Image, docker_tarball, gzip_stored, oci_layout, tar_of};
use crate::transfer::transfer;

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, bytes).unwrap();
    p
}

fn read_blob(src: &dyn Source, d: &Descriptor) -> Vec<u8> {
    let mut v = Vec::new();
    src.blob(d).unwrap().read_to_end(&mut v).unwrap();
    v
}

#[test]
fn tarball_image_builds_go_identical_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let img = Image::small(1);
    let p = write(dir.path(), "img.tar", &docker_tarball(&[("r:t", &img)]));
    let t = TarballImage::open(&p).unwrap();
    let want = docker_manifest_bytes(
        &img.config_descriptor(media::DOCKER_CONFIG),
        &img.layer_descriptors(media::DOCKER_LAYER),
    );
    assert_eq!(t.manifest, want);
    assert_eq!(read_blob(&t, &img.config_descriptor("x")), img.config);
    for (l, d) in img.layers.iter().zip(img.layer_descriptors("x")) {
        assert_eq!(&read_blob(&t, &d), l);
    }
    assert!(t.manifest("sha256:x").is_err());
    assert!(
        t.blob(&Descriptor::new("x", 1, &digest::of(b"nope")))
            .is_err()
    );
}

#[test]
fn tarball_symlinks_and_dot_slash_names() {
    let dir = tempfile::tempdir().unwrap();
    let img = Image::new(5, 1, 8, "linux", "amd64");
    let layer_name = format!("{}.tar.gz", &digest::of(&img.layers[0])[7..]);
    let manifest = r#"[{"Config":"cfg","RepoTags":null,"Layers":["link"]}]"#.to_string();
    let mut b = tar::Builder::new(Vec::new());
    for (name, data) in [
        ("./cfg", img.config.as_slice()),
        (layer_name.as_str(), img.layers[0].as_slice()),
        ("manifest.json", manifest.as_bytes()),
    ] {
        let mut h = tar::Header::new_ustar();
        h.set_size(data.len() as u64);
        h.set_entry_type(tar::EntryType::Regular);
        b.append_data(&mut h, name, data).unwrap();
    }
    let mut h = tar::Header::new_ustar();
    h.set_entry_type(tar::EntryType::Symlink);
    h.set_size(0);
    b.append_link(&mut h, "link", &layer_name).unwrap();
    let mut h = tar::Header::new_ustar();
    h.set_entry_type(tar::EntryType::Symlink);
    h.set_size(0);
    b.append_link(&mut h, "dangling", "nowhere").unwrap();
    let mut h = tar::Header::new_ustar();
    h.set_entry_type(tar::EntryType::Directory);
    h.set_size(0);
    b.append_data(&mut h, "somedir/", &[][..]).unwrap();
    let p = write(dir.path(), "x.tar", &b.into_inner().unwrap());
    let t = TarballImage::open(&p).unwrap();
    assert_eq!(
        t.manifest,
        docker_manifest_bytes(
            &img.config_descriptor(media::DOCKER_CONFIG),
            &img.layer_descriptors(media::DOCKER_LAYER)
        )
    );
}

#[test]
fn tarball_foreign_layers_come_from_layer_sources() {
    let dir = tempfile::tempdir().unwrap();
    let diff = digest::of(b"foreign-diff");
    let config = format!(
        r#"{{"architecture":"amd64","os":"windows","rootfs":{{"type":"layers","diff_ids":["{diff}"]}}}}"#
    );
    let mut fd = Descriptor::new(
        "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip",
        9,
        &digest::of(b"f"),
    );
    fd.urls = vec!["https://example.invalid/l".into()];
    let entry = serde_json::json!([{"Config":"c","RepoTags":null,"Layers":["missing.tar.gz"],"LayerSources":{diff.clone(): fd}}]);
    let m = serde_json::to_vec(&entry).unwrap();
    let p = write(
        dir.path(),
        "f.tar",
        &tar_of(&[("c", config.as_bytes()), ("manifest.json", &m)]),
    );
    let t = TarballImage::open(&p).unwrap();
    let v = crate::types::ManifestView::parse(&t.manifest).unwrap();
    assert_eq!(v.layers, vec![fd]);
}

#[test]
fn tarball_errors() {
    let dir = tempfile::tempdir().unwrap();
    let img = Image::small(2);
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "two",
            docker_tarball(&[("a:b", &img), ("c:d", &img)]),
            "single image",
        ),
        (
            "nomanifest",
            tar_of(&[("x", b"y")]),
            "manifest.json not found",
        ),
        (
            "badmanifest",
            tar_of(&[("manifest.json", b"{")]),
            "parsing manifest.json",
        ),
        (
            "noconfig",
            tar_of(&[(
                "manifest.json",
                br#"[{"Config":"c","RepoTags":null,"Layers":[]}]"#,
            )]),
            "c not found",
        ),
        (
            "badconfig",
            tar_of(&[
                (
                    "manifest.json",
                    br#"[{"Config":"c","RepoTags":null,"Layers":[]}]"#,
                ),
                ("c", b"["),
            ]),
            "image config",
        ),
        (
            "fewdiffids",
            tar_of(&[
                (
                    "manifest.json",
                    br#"[{"Config":"c","RepoTags":null,"Layers":["l"]}]"#,
                ),
                ("c", br#"{"rootfs":{"diff_ids":[]}}"#),
            ]),
            "rootfs.diff_ids",
        ),
        (
            "nolayer",
            tar_of(&[
                (
                    "manifest.json",
                    br#"[{"Config":"c","RepoTags":null,"Layers":["l"]}]"#,
                ),
                ("c", br#"{"rootfs":{"diff_ids":["x"]}}"#),
            ]),
            "l not found",
        ),
        (
            "short",
            tar_of(&[
                (
                    "manifest.json",
                    br#"[{"Config":"c","RepoTags":null,"Layers":["l"]}]"#,
                ),
                ("c", br#"{"rootfs":{"diff_ids":["x"]}}"#),
                ("l", b"x"),
            ]),
            "too short",
        ),
        (
            "uncompressed",
            tar_of(&[
                (
                    "manifest.json",
                    br#"[{"Config":"c","RepoTags":null,"Layers":["l"]}]"#,
                ),
                ("c", br#"{"rootfs":{"diff_ids":["x"]}}"#),
                ("l", b"plain tar"),
            ]),
            "not gzip",
        ),
        ("notatar", vec![1; 10], "reading"),
    ];
    for (name, bytes, want) in cases {
        let p = write(dir.path(), name, &bytes);
        let err = format!(
            "{:#}",
            TarballImage::open(&p)
                .err()
                .unwrap_or_else(|| panic!("{name} opened"))
        );
        assert!(err.contains(want), "{name}: {err}");
    }
    assert!(TarballImage::open(&dir.path().join("missing")).is_err());
    // keep gzip helper referenced for the "is gzip" happy path
    assert_eq!(&gzip_stored(b"a")[..2], &[0x1f, 0x8b]);
}

#[test]
fn layout_create_open_index() {
    let dir = tempfile::tempdir().unwrap();
    assert!(Layout::open(dir.path()).is_err());
    let l = Layout::create(&dir.path().join("new")).unwrap();
    assert_eq!(l.index().unwrap().manifests.len(), 0);
    // create on an existing layout opens it
    let again = Layout::create(&dir.path().join("new")).unwrap();
    assert!(again.index_bytes().unwrap().starts_with(
        br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json""#
    ));
    assert!(l.blob_path("bad").is_err());
    assert!(l.manifest(&digest::of(b"none")).is_err());
    assert!(
        l.blob(&Descriptor::new("x", 1, &digest::of(b"none")))
            .is_err()
    );
    fs::write(dir.path().join("new/index.json"), b"nope").unwrap();
    assert!(l.index().is_err());
}

#[test]
fn layout_round_trip_through_transfer() {
    let src_dir = tempfile::tempdir().unwrap();
    let (a, b) = (Image::small(1), Image::new(2, 1, 16, "linux", "arm64"));
    let root = oci_layout(src_dir.path(), &[&a, &b], true).unwrap();
    let src = Layout::open(src_dir.path()).unwrap();
    let idx = src.index().unwrap();
    let bytes = src.manifest(&root).unwrap();
    let dst_dir = tempfile::tempdir().unwrap();
    let dst = Layout::create(dst_dir.path()).unwrap();
    let d = transfer(
        &src,
        &dst,
        &bytes,
        &idx.manifests[0].media_type,
        "t",
        &mut HashSet::new(),
    )
    .unwrap();
    assert_eq!(d, root);
    assert!(dst.has_blob(&digest::of(&b.layers[0])).unwrap());
}

/// A source whose blob bytes do not match the descriptor.
struct Liar;
impl Source for Liar {
    fn manifest(&self, _: &str) -> Result<Vec<u8>> {
        unreachable!()
    }
    fn blob(&self, _: &Descriptor) -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(Cursor::new(b"lies".to_vec())))
    }
}

/// A source whose reader fails midway.
struct Broken;
impl Source for Broken {
    fn manifest(&self, _: &str) -> Result<Vec<u8>> {
        unreachable!()
    }
    fn blob(&self, _: &Descriptor) -> Result<Box<dyn Read + Send>> {
        struct R;
        impl Read for R {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("broken"))
            }
        }
        Ok(Box::new(R))
    }
}

#[test]
fn layout_put_blob_rejects_bad_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let l = Layout::create(dir.path()).unwrap();
    let d = Descriptor::new("x", 4, &digest::of(b"true"));
    assert!(format!("{:#}", l.put_blob(&d, &Liar).unwrap_err()).contains("mismatch"));
    assert!(l.put_blob(&d, &Broken).is_err());
    assert!(!l.has_blob(&d.digest).unwrap());
    assert!(
        !l.blob_path(&d.digest)
            .unwrap()
            .with_extension("tmp")
            .exists()
    );
}

#[test]
fn atomic_write_errors() {
    let dir = tempfile::tempdir().unwrap();
    assert!(atomic_write(&dir.path().join("no/such/dir/f"), b"x").is_err());
}

#[test]
fn tarball_writer_round_trip_and_dedup() {
    let dir = tempfile::tempdir().unwrap();
    let img = Image::small(9);
    let p = dir.path().join("out.tar");
    let mut w = TarballWriter::create(&p).unwrap();
    let layers = img.layer_descriptors(media::DOCKER_LAYER);
    let blobs = img.layers.clone();
    let mut open = |d: &Descriptor| -> Result<Box<dyn Read + Send>> {
        let i = layers.iter().position(|x| x.digest == d.digest).unwrap();
        Ok(Box::new(Cursor::new(blobs[i].clone())))
    };
    w.add_image("r:a", &img.config, &layers, &mut open).unwrap();
    w.add_image("r:b", &img.config, &layers, &mut open).unwrap(); // same blobs: written once
    w.finish().unwrap();
    let files = index_tar(&p).unwrap();
    assert_eq!(files.len(), 1 + layers.len() + 1);
    // the written tarball pushes back identically
    let mut single = TarballWriter::create(&dir.path().join("one.tar")).unwrap();
    single
        .add_image("r:a", &img.config, &layers, &mut open)
        .unwrap();
    single.finish().unwrap();
    let t = TarballImage::open(&dir.path().join("one.tar")).unwrap();
    assert_eq!(
        t.manifest,
        docker_manifest_bytes(&img.config_descriptor(media::DOCKER_CONFIG), &layers)
    );
}

#[test]
fn tarball_writer_errors() {
    let dir = tempfile::tempdir().unwrap();
    assert!(TarballWriter::create(&dir.path().join("no/dir/x.tar")).is_err());
    let img = Image::small(3);
    let mut layers = img.layer_descriptors(media::DOCKER_LAYER);
    let blobs = img.layers.clone();
    let mut wrong = |_: &Descriptor| -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(Cursor::new(blobs[1].clone())))
    };
    let p = dir.path().join("x.tar");
    let mut w = TarballWriter::create(&p).unwrap();
    assert!(
        w.add_image("r:a", &img.config, &layers[..1], &mut wrong)
            .is_err()
    );
    w.abort();
    assert!(!PathBuf::from(format!("{}.tmp", p.display())).exists());
    let mut w = TarballWriter::create(&p).unwrap();
    layers[0].urls = vec!["u".into()];
    assert!(
        w.add_image("r:a", &img.config, &layers, &mut wrong)
            .unwrap_err()
            .to_string()
            .contains("foreign")
    );
    layers[0].urls.clear();
    layers[0].digest = "bad".into();
    assert!(
        w.add_image("r:a", &img.config, &layers, &mut wrong)
            .unwrap_err()
            .to_string()
            .contains("invalid digest")
    );
    let mut failing = |_: &Descriptor| -> Result<Box<dyn Read + Send>> { anyhow::bail!("nope") };
    layers[0] = img.layer_descriptors(media::DOCKER_LAYER)[0].clone();
    assert!(
        w.add_image("r:a", &img.config, &layers, &mut failing)
            .is_err()
    );
    w.abort();
}
