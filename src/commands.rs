//! Client commands, matching go crane v0.22.1 behaviour and output for the v1 subset.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::auth::Keychain;
use crate::cli::Format;
use crate::client::{Client, Fetched};
use crate::digest::HashingReader;
use crate::local::{Layout, TarballImage, TarballWriter};
use crate::reference::{self, Reference, Repository};
use crate::transfer::{RemoteRepo, Source, transfer};
use crate::types::{Descriptor, ManifestView, Platform, media};

/// go crane tags digest references with this when writing a tarball.
pub const I_WAS_A_DIGEST: &str = "i-was-a-digest";
pub const REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";

pub struct Ctx {
    pub platform: Option<Platform>,
    pub client: Client,
}

impl Ctx {
    pub fn new(insecure: bool, platform: Option<&str>, verbose: bool) -> Result<Self> {
        Self::with_keychain(insecure, platform, verbose, Keychain::from_env()?)
    }

    pub fn with_keychain(
        insecure: bool,
        platform: Option<&str>,
        verbose: bool,
        keychain: Keychain,
    ) -> Result<Self> {
        let platform = match platform {
            None | Some("all") => None,
            Some(p) => Some(Platform::parse(p)?),
        };
        Ok(Self {
            platform,
            client: Client::new(insecure, verbose, keychain),
        })
    }

    /// The platform used to pick an image out of an index (go default: linux/amd64).
    fn image_platform(&self) -> Platform {
        self.platform
            .clone()
            .unwrap_or_else(Platform::default_linux_amd64)
    }

    /// The manifest at `r`, resolved to a single image if it is an index.
    fn resolve_image(&self, r: &Reference) -> Result<(Reference, Fetched, ManifestView)> {
        let fetched = self.client.get_manifest(r)?;
        let view = ManifestView::parse(&fetched.bytes)?;
        if !view.is_index(&fetched.content_type) {
            return Ok((r.clone(), fetched, view));
        }
        let child = view.pick(&self.image_platform())?;
        let child_ref = r.repo.digest(&child.digest);
        let mut fetched = self.client.get_manifest(&child_ref)?;
        if fetched.content_type.is_empty() {
            fetched.content_type = child.media_type.clone();
        }
        let view = ManifestView::parse(&fetched.bytes)?;
        Ok((child_ref, fetched, view))
    }

    fn config_bytes(&self, repo: &Repository, view: &ManifestView) -> Result<Vec<u8>> {
        let cfg = view.config.as_ref().context("manifest has no config")?;
        read_verified(self.client.get_blob(repo, &cfg.digest)?, &cfg.digest)
    }
}

fn read_verified(r: Box<dyn Read + Send>, expected: &str) -> Result<Vec<u8>> {
    let mut r = HashingReader::new(r);
    let mut buf = Vec::new();
    r.read_to_end(&mut buf)?;
    let (got, _) = r.finish();
    if got != expected {
        bail!("blob digest mismatch: expected {expected}, got {got}");
    }
    Ok(buf)
}

fn local_source(path: &Path, index: bool) -> Result<(Box<dyn Source>, Vec<u8>, String)> {
    let meta = std::fs::metadata(path).with_context(|| format!("{}", path.display()))?;
    if !meta.is_dir() {
        let img = TarballImage::open(path)
            .with_context(|| format!("loading {} as tarball", path.display()))?;
        let m = img.manifest.clone();
        return Ok((Box::new(img), m, media::DOCKER_MANIFEST.into()));
    }
    let layout =
        Layout::open(path).with_context(|| format!("loading {} as OCI layout", path.display()))?;
    if index {
        let bytes = layout.index_bytes()?;
        return Ok((Box::new(layout), bytes, media::OCI_INDEX.into()));
    }
    let idx = layout.index()?;
    if idx.manifests.len() != 1 {
        bail!(
            "layout contains {} entries, consider --index",
            idx.manifests.len()
        );
    }
    let desc = &idx.manifests[0];
    let mt = desc.media_type.as_str();
    if !(media::is_index(mt) || mt == media::OCI_MANIFEST || mt == media::DOCKER_MANIFEST) {
        bail!("layout contains non-image (mediaType: {mt:?}), consider --index");
    }
    let bytes = layout.manifest(&desc.digest)?;
    let ct = desc.media_type.clone();
    Ok((Box::new(layout), bytes, ct))
}

/// Push a docker-save tarball or OCI layout. Prints "<registry>/<repo>@<digest>".
pub fn push(
    ctx: &Ctx,
    path: &Path,
    reference: &str,
    index: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let (src, bytes, ct) = local_source(path, index)?;
    let r = Reference::parse(reference)?;
    let dst = RemoteRepo {
        client: &ctx.client,
        repo: r.repo.clone(),
    };
    let d = transfer(
        src.as_ref(),
        &dst,
        &bytes,
        &ct,
        r.identifier(),
        &mut HashSet::new(),
    )?;
    writeln!(out, "{}", r.repo.digest(&d))?;
    Ok(())
}

/// Pull images to a tarball (default) or an OCI layout.
pub fn pull(
    ctx: &Ctx,
    refs: &[String],
    path: &Path,
    format: Format,
    annotate_ref: bool,
) -> Result<()> {
    let refs = refs
        .iter()
        .map(|s| Reference::parse(s).with_context(|| format!("parsing reference {s:?}")))
        .collect::<Result<Vec<_>>>()?;
    match format {
        Format::Tarball => {
            let mut w = TarballWriter::create(path)?;
            let res = (|| -> Result<()> {
                for r in &refs {
                    let (_, _, view) = ctx.resolve_image(r)?;
                    let config = ctx.config_bytes(&r.repo, &view)?;
                    let tag = if r.is_digest() {
                        r.repo.tag(I_WAS_A_DIGEST).to_string()
                    } else {
                        r.to_string()
                    };
                    let repo = r.repo.clone();
                    w.add_image(&tag, &config, &view.layers, &mut |d: &Descriptor| {
                        ctx.client.get_blob(&repo, &d.digest)
                    })?;
                }
                Ok(())
            })();
            match res {
                Ok(()) => w.finish(),
                Err(e) => {
                    w.abort();
                    Err(e).with_context(|| format!("saving tarball {}", path.display()))
                }
            }
        }
        Format::Oci => {
            let layout = Layout::create(path)?;
            for r in &refs {
                let fetched = ctx.client.get_manifest(r)?;
                let whole_index = ctx.platform.is_none()
                    && ManifestView::parse(&fetched.bytes)?.is_index(&fetched.content_type);
                let (bytes, ct) = if whole_index {
                    (fetched.bytes, fetched.content_type)
                } else {
                    let (_, f, _) = ctx.resolve_image(r)?;
                    (f.bytes, f.content_type)
                };
                let src = RemoteRepo {
                    client: &ctx.client,
                    repo: r.repo.clone(),
                };
                let d = transfer(
                    &src,
                    &layout,
                    &bytes,
                    &ct,
                    r.identifier(),
                    &mut HashSet::new(),
                )?;
                let view = ManifestView::parse(&bytes)?;
                let mut desc = Descriptor::new(view.content_type(&ct), bytes.len() as u64, &d);
                if annotate_ref {
                    desc.annotations
                        .insert(REF_NAME_ANNOTATION.into(), r.to_string());
                }
                let mut idx = layout.index()?;
                idx.manifests.push(desc);
                layout.write_index(&idx)?;
            }
            Ok(())
        }
    }
}

/// Re-PUT the manifest bytes of `image` under `tag`.
pub fn tag(ctx: &Ctx, image: &str, tag: &str) -> Result<()> {
    let r = Reference::parse(image)?;
    let new = Reference::parse(&format!("{}:{tag}", r.repo.name()))?;
    let f = ctx.client.get_manifest(&r)?;
    let ct = ManifestView::parse(&f.bytes)?
        .content_type(&f.content_type)
        .to_string();
    ctx.client
        .put_manifest(&new.repo, new.identifier(), &f.bytes, &ct)?;
    Ok(())
}

/// Copy an image or index (or one platform's image with --platform), digests kept.
pub fn copy(ctx: &Ctx, src: &str, dst: &str) -> Result<()> {
    let s = Reference::parse(src)?;
    let d = Reference::parse(dst)?;
    let fetched = if ctx.platform.is_some() {
        ctx.resolve_image(&s)?.1
    } else {
        ctx.client.get_manifest(&s)?
    };
    let source = RemoteRepo {
        client: &ctx.client,
        repo: s.repo.clone(),
    };
    let sink = RemoteRepo {
        client: &ctx.client,
        repo: d.repo.clone(),
    };
    transfer(
        &source,
        &sink,
        &fetched.bytes,
        &fetched.content_type,
        d.identifier(),
        &mut HashSet::new(),
    )?;
    Ok(())
}

pub fn digest(ctx: &Ctx, image: &str, full_ref: bool, out: &mut dyn Write) -> Result<()> {
    let r = Reference::parse(image)?;
    let d = if ctx.platform.is_some() {
        ctx.resolve_image(&r)?.1.digest
    } else {
        match ctx.client.head_manifest(&r) {
            Ok(Some(h)) => h.digest,
            _ => ctx.client.get_manifest(&r)?.digest,
        }
    };
    if full_ref {
        writeln!(out, "{}", r.repo.digest(&d))?;
    } else {
        writeln!(out, "{d}")?;
    }
    Ok(())
}

pub fn manifest(ctx: &Ctx, image: &str, out: &mut dyn Write) -> Result<()> {
    let r = Reference::parse(image)?;
    let bytes = if ctx.platform.is_some() {
        ctx.resolve_image(&r)?.1.bytes
    } else {
        ctx.client.get_manifest(&r)?.bytes
    };
    out.write_all(&bytes)?;
    Ok(())
}

pub fn config(ctx: &Ctx, image: &str, out: &mut dyn Write) -> Result<()> {
    let r = Reference::parse(image)?;
    let (img, _, view) = ctx.resolve_image(&r)?;
    out.write_all(&ctx.config_bytes(&img.repo, &view)?)?;
    Ok(())
}

pub fn blob(ctx: &Ctx, blob: &str, out: &mut dyn Write) -> Result<()> {
    let r = Reference::parse(blob)?;
    if !r.is_digest() {
        bail!("blob reference must be a digest (repo@sha256:...): {blob}");
    }
    let mut reader = HashingReader::new(ctx.client.get_blob(&r.repo, r.identifier())?);
    std::io::copy(&mut reader, out)?;
    let (got, _) = reader.finish();
    if got != r.identifier() {
        bail!(
            "blob digest mismatch: expected {}, got {got}",
            r.identifier()
        );
    }
    Ok(())
}

pub fn ls(
    ctx: &Ctx,
    repo: &str,
    full_ref: bool,
    omit_digest_tags: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let repo = Repository::parse(repo)?;
    for t in ctx.client.tags(&repo)? {
        if omit_digest_tags && t.starts_with("sha256-") {
            continue;
        }
        if full_ref {
            writeln!(out, "{}", repo.tag(&t))?;
        } else {
            writeln!(out, "{t}")?;
        }
    }
    Ok(())
}

pub fn catalog(ctx: &Ctx, registry: &str, full_ref: bool, out: &mut dyn Write) -> Result<()> {
    let reg = reference::parse_registry(registry)?;
    for r in ctx.client.catalog(&reg)? {
        if full_ref {
            writeln!(out, "{registry}/{r}")?;
        } else {
            writeln!(out, "{r}")?;
        }
    }
    Ok(())
}

pub fn delete(ctx: &Ctx, image: &str) -> Result<()> {
    ctx.client.delete_manifest(&Reference::parse(image)?)
}

#[cfg(test)]
pub(crate) mod tests;
