//! OCI Distribution v1.1 routes over a `Store`: path parsing, validation, handlers.

use std::fmt::Display;
use std::io::{self, Read, Seek, SeekFrom};

use serde_json::{Value, json};

use super::http::{Payload, Request, Response};
use super::store::{self, Store};
use crate::digest;
use crate::types::{Catalog, ErrorItem, ErrorResponse, ManifestView, TagList, media};

/// Largest manifest accepted by PUT.
pub const MAX_MANIFEST: u64 = 4 * 1024 * 1024;

const JSON: &str = "application/json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Base,
    Catalog,
    Blob { name: String, digest: String },
    UploadStart { name: String },
    Upload { name: String, id: String },
    Manifest { name: String, reference: String },
    Tags { name: String },
    Referrers { name: String, digest: String },
}

/// Map a decoded path to a route. Repository names may contain '/', so the
/// route is read from the last segments.
pub fn parse_route(path: &str) -> Option<Route> {
    let rest = path.strip_prefix("/v2")?;
    if rest.is_empty() || rest == "/" {
        return Some(Route::Base);
    }
    let rest = rest.strip_prefix('/')?;
    if rest == "_catalog" {
        return Some(Route::Catalog);
    }
    let segs: Vec<&str> = rest.split('/').collect();
    let n = segs.len();
    let name = |k: usize| segs[..n - k].join("/");
    let last = segs[n - 1].to_string();
    Some(match segs.as_slice() {
        [_, .., "tags", "list"] => Route::Tags { name: name(2) },
        [_, .., "blobs", "uploads", ""] => Route::UploadStart { name: name(3) },
        [_, .., "blobs", "uploads"] => Route::UploadStart { name: name(2) },
        [_, .., "blobs", "uploads", _] => Route::Upload {
            name: name(3),
            id: last,
        },
        [_, .., "blobs", _] => Route::Blob {
            name: name(2),
            digest: last,
        },
        [_, .., "manifests", _] => Route::Manifest {
            name: name(2),
            reference: last,
        },
        [_, .., "referrers", _] => Route::Referrers {
            name: name(2),
            digest: last,
        },
        _ => return None,
    })
}

/// One name component: `[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*`.
fn is_component(c: &str) -> bool {
    let b = c.as_bytes();
    let alnum = |x: u8| x.is_ascii_lowercase() || x.is_ascii_digit();
    if b.is_empty() || !alnum(b[0]) || !alnum(b[b.len() - 1]) {
        return false;
    }
    // Every run of separators must be ".", "_", "__" or "-+".
    let mut i = 0;
    while i < b.len() {
        if alnum(b[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while !alnum(b[i]) {
            i += 1;
        }
        let sep = &c[start..i];
        if !(sep == "." || sep == "_" || sep == "__" || sep.bytes().all(|x| x == b'-')) {
            return false;
        }
    }
    true
}

/// Repository name per the spec regex, at most 255 chars.
pub fn is_name(name: &str) -> bool {
    name.len() <= 255 && name.split('/').all(is_component)
}

/// `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`
pub fn is_tag(tag: &str) -> bool {
    let b = tag.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && (b[0].is_ascii_alphanumeric() || b[0] == b'_')
        && b.iter()
            .all(|&x| x.is_ascii_alphanumeric() || b"._-".contains(&x))
}

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub headers: Vec<(&'static str, String)>,
}

pub fn err(status: u16, code: &'static str, message: impl Display) -> ApiError {
    ApiError {
        status,
        code,
        message: message.to_string(),
        headers: Vec::new(),
    }
}

impl From<io::Error> for ApiError {
    fn from(e: io::Error) -> Self {
        err(500, "UNKNOWN", format!("storage error: {e}"))
    }
}

impl ApiError {
    pub fn into_response(self) -> Response {
        let body = ErrorResponse {
            errors: vec![ErrorItem {
                code: self.code.to_string(),
                message: self.message,
                detail: None,
            }],
        };
        let mut resp = Response::new(self.status);
        resp.headers = self.headers;
        resp.bytes(JSON, serde_json::to_vec(&body).expect("error serializes"))
    }
}

fn unsupported() -> ApiError {
    err(405, "UNSUPPORTED", "unsupported method or route")
}
fn blob_unknown() -> ApiError {
    err(404, "BLOB_UNKNOWN", "blob unknown to registry")
}
fn upload_unknown() -> ApiError {
    err(
        404,
        "BLOB_UPLOAD_UNKNOWN",
        "blob upload unknown to registry",
    )
}
fn manifest_unknown() -> ApiError {
    err(404, "MANIFEST_UNKNOWN", "manifest unknown to registry")
}

fn check_name(name: &str) -> Result<&str, ApiError> {
    if is_name(name) {
        Ok(name)
    } else {
        Err(err(
            400,
            "NAME_INVALID",
            format!("invalid repository name {name:?}"),
        ))
    }
}

/// A sha256 digest; other well-formed algorithms are UNSUPPORTED.
pub fn parse_digest(d: &str) -> Result<String, ApiError> {
    if digest::is_valid(d) {
        return Ok(d.to_string());
    }
    let other_algorithm = d.split_once(':').is_some_and(|(alg, enc)| {
        alg != "sha256"
            && !alg.is_empty()
            && alg
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"+._-".contains(&b))
            && !enc.is_empty()
            && enc
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"=_-".contains(&b))
    });
    if other_algorithm {
        Err(err(
            400,
            "UNSUPPORTED",
            format!("unsupported digest algorithm: {d}"),
        ))
    } else {
        Err(err(400, "DIGEST_INVALID", format!("invalid digest {d:?}")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    Tag(String),
    Digest(String),
}

pub fn parse_reference(r: &str) -> Result<Reference, ApiError> {
    if r.contains(':') {
        parse_digest(r).map(Reference::Digest)
    } else if is_tag(r) {
        Ok(Reference::Tag(r.to_string()))
    } else {
        Err(err(400, "TAG_INVALID", format!("invalid tag {r:?}")))
    }
}

fn upload_id(id: &str) -> Result<&str, ApiError> {
    if store::is_upload_id(id) {
        Ok(id)
    } else {
        Err(upload_unknown())
    }
}

/// `Range: bytes=a-b | bytes=a- | bytes=-n` against a body of `size` bytes:
/// the inclusive (start, end), or None if malformed or unsatisfiable.
pub fn parse_range(h: &str, size: u64) -> Option<(u64, u64)> {
    let (a, b) = h.strip_prefix("bytes=")?.split_once('-')?;
    let last = size.checked_sub(1)?;
    let (start, end) = match (a.parse::<u64>(), b.parse::<u64>()) {
        // bytes=-n: the last n bytes (n = 0 gives start = size > last: rejected below).
        _ if a.is_empty() => (size.saturating_sub(b.parse().ok()?), last),
        (Ok(start), _) if b.is_empty() => (start, last),
        (Ok(start), Ok(end)) => (start, end.min(last)),
        _ => return None,
    };
    (start <= end).then_some((start, end))
}

/// The start offset of a `Content-Range: [bytes ]<start>-<end>[/<total>]`.
pub fn content_range_start(h: &str) -> Option<u64> {
    let h = h
        .strip_prefix("bytes")
        .unwrap_or(h)
        .trim_start_matches([' ', '=']);
    let range = h.split('/').next().unwrap_or_default();
    let (a, b) = range.split_once('-')?;
    let (start, end): (u64, u64) = (a.parse().ok()?, b.parse().ok()?);
    (start <= end).then_some(start)
}

/// Handle one request. `body` is the request body (may be left unread).
pub fn handle(store: &Store, req: &Request, body: &mut dyn Read) -> Response {
    dispatch(store, req, body).unwrap_or_else(ApiError::into_response)
}

fn dispatch(store: &Store, req: &Request, body: &mut dyn Read) -> Result<Response, ApiError> {
    let route = parse_route(&req.path).ok_or_else(unsupported)?;
    let method = req.method.as_str();
    let get = method == "GET" || method == "HEAD";
    match route {
        Route::Base if get => Ok(Response::new(200).bytes(JSON, b"{}".to_vec())),
        Route::Catalog if get => catalog(store, req),
        Route::Blob { name, digest } => {
            let name = check_name(&name)?;
            let digest = parse_digest(&digest)?;
            match method {
                _ if get => get_blob(store, req, name, &digest),
                "DELETE" => delete_blob(store, name, &digest),
                _ => Err(unsupported()),
            }
        }
        Route::UploadStart { name } if method == "POST" => {
            start_upload(store, req, check_name(&name)?, body)
        }
        Route::Upload { name, id } => {
            let name = check_name(&name)?;
            let id = upload_id(&id)?;
            match method {
                _ if get => upload_status(store, name, id),
                "PATCH" => patch_upload(store, req, name, id, body),
                "PUT" => put_upload(store, req, name, id, body),
                "DELETE" => cancel_upload(store, id),
                _ => Err(unsupported()),
            }
        }
        Route::Manifest { name, reference } => {
            let name = check_name(&name)?;
            let reference = parse_reference(&reference)?;
            match method {
                _ if get => get_manifest(store, name, &reference),
                "PUT" => put_manifest(store, req, name, &reference, body),
                "DELETE" => delete_manifest(store, name, &reference),
                _ => Err(unsupported()),
            }
        }
        Route::Tags { name } if get => tags_list(store, req, check_name(&name)?),
        Route::Referrers { name, digest } if get => {
            let name = check_name(&name)?;
            referrers(store, req, name, &parse_digest(&digest)?)
        }
        _ => Err(unsupported()),
    }
}

// ---- blobs

fn blob_created(name: &str, digest: &str) -> Response {
    Response::new(201)
        .header("Location", format!("/v2/{name}/blobs/{digest}"))
        .header("Docker-Content-Digest", digest)
}

fn get_blob(store: &Store, req: &Request, name: &str, digest: &str) -> Result<Response, ApiError> {
    let (mut file, size) = store.open_blob(name, digest)?.ok_or_else(blob_unknown)?;
    let mut resp = Response::new(200)
        .header("Content-Type", "application/octet-stream")
        .header("Docker-Content-Digest", digest)
        .header("Accept-Ranges", "bytes");
    let mut len = size;
    if let Some(range) = req.header("range") {
        let (start, end) = parse_range(range, size).ok_or_else(|| {
            let mut e = err(
                416,
                "BLOB_UNKNOWN",
                format!("range {range:?} not satisfiable"),
            );
            e.headers.push(("Content-Range", format!("bytes */{size}")));
            e
        })?;
        file.seek(SeekFrom::Start(start))?;
        len = end - start + 1;
        resp.status = 206;
        resp = resp.header("Content-Range", format!("bytes {start}-{end}/{size}"));
    }
    resp.payload = Payload::File(file, len);
    Ok(resp)
}

fn delete_blob(store: &Store, name: &str, digest: &str) -> Result<Response, ApiError> {
    if store.unlink_blob(name, digest)? {
        Ok(Response::new(202))
    } else {
        Err(blob_unknown())
    }
}

// ---- uploads

fn upload_accepted(status: u16, name: &str, id: &str, len: u64) -> Response {
    Response::new(status)
        .header("Location", format!("/v2/{name}/blobs/uploads/{id}"))
        .header("Range", format!("0-{}", len.saturating_sub(1)))
        .header("Docker-Upload-UUID", id)
}

fn start_upload(
    store: &Store,
    req: &Request,
    name: &str,
    body: &mut dyn Read,
) -> Result<Response, ApiError> {
    if let (Some(mount), Some(from)) = (req.query("mount"), req.query("from")) {
        let digest = parse_digest(mount)?;
        if is_name(from) && store.has_blob_link(from, &digest) {
            store.link_blob(name, &digest)?;
            return Ok(blob_created(name, &digest));
        }
    }
    let digest = req.query("digest").map(parse_digest).transpose()?;
    let id = store.new_upload()?;
    match digest {
        Some(digest) => complete_upload(store, name, &id, &digest, body),
        None => Ok(upload_accepted(202, name, &id, 0)),
    }
}

fn upload_status(store: &Store, name: &str, id: &str) -> Result<Response, ApiError> {
    let len = store.upload_len(id)?.ok_or_else(upload_unknown)?;
    Ok(upload_accepted(204, name, id, len))
}

fn patch_upload(
    store: &Store,
    req: &Request,
    name: &str,
    id: &str,
    body: &mut dyn Read,
) -> Result<Response, ApiError> {
    let len = store.upload_len(id)?.ok_or_else(upload_unknown)?;
    if let Some(cr) = req.header("content-range")
        && content_range_start(cr) != Some(len)
    {
        let mut e = err(
            416,
            "BLOB_UPLOAD_INVALID",
            format!("Content-Range {cr:?} does not start at offset {len}"),
        );
        e.headers
            .push(("Range", format!("0-{}", len.saturating_sub(1))));
        return Err(e);
    }
    let len = store.append_upload(id, body)?.ok_or_else(upload_unknown)?;
    Ok(upload_accepted(202, name, id, len))
}

fn put_upload(
    store: &Store,
    req: &Request,
    name: &str,
    id: &str,
    body: &mut dyn Read,
) -> Result<Response, ApiError> {
    let digest = parse_digest(req.query("digest").unwrap_or_default())?;
    complete_upload(store, name, id, &digest, body)
}

/// Append the last bytes, verify the digest, store the blob and link it.
fn complete_upload(
    store: &Store,
    name: &str,
    id: &str,
    digest: &str,
    body: &mut dyn Read,
) -> Result<Response, ApiError> {
    store.append_upload(id, body)?.ok_or_else(upload_unknown)?;
    let (got, _) = store.upload_digest(id)?.ok_or_else(upload_unknown)?;
    if got != digest {
        store.cancel_upload(id)?;
        return Err(err(
            400,
            "DIGEST_INVALID",
            format!("upload has digest {got}, not {digest}"),
        ));
    }
    store.commit_upload(id, digest)?;
    store.link_blob(name, digest)?;
    Ok(blob_created(name, digest))
}

fn cancel_upload(store: &Store, id: &str) -> Result<Response, ApiError> {
    if store.cancel_upload(id)? {
        Ok(Response::new(204))
    } else {
        Err(upload_unknown())
    }
}

// ---- manifests

fn get_manifest(store: &Store, name: &str, reference: &Reference) -> Result<Response, ApiError> {
    let digest = match reference {
        Reference::Digest(d) => d.clone(),
        Reference::Tag(t) => store.tag_digest(name, t)?.ok_or_else(manifest_unknown)?,
    };
    let m = store
        .manifest(name, &digest)?
        .ok_or_else(manifest_unknown)?;
    Ok(Response::new(200)
        .header("Docker-Content-Digest", m.digest)
        .bytes(&m.content_type, m.bytes))
}

fn manifest_invalid(message: impl Display) -> ApiError {
    err(400, "MANIFEST_INVALID", message)
}

fn put_manifest(
    store: &Store,
    req: &Request,
    name: &str,
    reference: &Reference,
    body: &mut dyn Read,
) -> Result<Response, ApiError> {
    let mut bytes = Vec::new();
    body.take(MAX_MANIFEST + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANIFEST {
        return Err(err(413, "SIZE_INVALID", "manifest is over 4 MiB"));
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(manifest_invalid)?;
    if !value.is_object() {
        return Err(manifest_invalid("manifest is not a JSON object"));
    }
    let view: ManifestView = serde_json::from_value(value).map_err(manifest_invalid)?;
    let content_type = match req.header("content-type").filter(|c| !c.is_empty()) {
        Some(c) => c.to_string(),
        None => view
            .media_type
            .clone()
            .filter(|m| !m.is_empty())
            .ok_or_else(|| manifest_invalid("no Content-Type and no mediaType"))?,
    };
    let digest = digest::of(&bytes);
    if let Reference::Digest(want) = reference
        && *want != digest
    {
        return Err(err(
            400,
            "DIGEST_INVALID",
            format!("manifest has digest {digest}, not {want}"),
        ));
    }
    let missing = if view.is_index(&content_type) {
        view.manifests
            .iter()
            .flatten()
            .find(|d| !store.has_manifest(name, &d.digest))
    } else {
        view.blobs()
            .into_iter()
            .find(|d| !d.is_foreign() && !store.has_blob_link(name, &d.digest))
    };
    if let Some(d) = missing {
        return Err(err(
            400,
            "MANIFEST_BLOB_UNKNOWN",
            format!("{} is not in {name}", d.digest),
        ));
    }
    let tag = match reference {
        Reference::Tag(t) => Some(t.as_str()),
        Reference::Digest(_) => None,
    };
    store.put_manifest(name, &bytes, &content_type, tag)?;
    let mut resp = Response::new(201)
        .header("Location", format!("/v2/{name}/manifests/{digest}"))
        .header("Docker-Content-Digest", digest);
    if let Some(subject) = &view.subject {
        resp = resp.header("OCI-Subject", subject.digest.clone());
    }
    Ok(resp)
}

fn delete_manifest(store: &Store, name: &str, reference: &Reference) -> Result<Response, ApiError> {
    let deleted = match reference {
        Reference::Digest(d) => store.delete_manifest(name, d)?,
        Reference::Tag(t) => store.delete_tag(name, t)?,
    };
    if deleted {
        Ok(Response::new(202))
    } else {
        Err(manifest_unknown())
    }
}

// ---- listings

/// Apply `last` and `n` to a sorted list. Returns the page and, when
/// truncated, the query for the next page.
fn paginate(req: &Request, items: Vec<String>) -> Result<(Vec<String>, Option<String>), ApiError> {
    let n = match req.query("n") {
        None => None,
        Some(s) => Some(s.parse::<usize>().map_err(|_| {
            err(
                400,
                "UNSUPPORTED",
                format!("n must be a non-negative integer, got {s:?}"),
            )
        })?),
    };
    let last = req.query("last");
    let mut items: Vec<String> = items
        .into_iter()
        .filter(|i| last.is_none_or(|l| i.as_str() > l))
        .collect();
    let mut next = None;
    if let Some(n) = n
        && items.len() > n
    {
        items.truncate(n);
        if let Some(l) = items.last() {
            next = Some(format!("n={n}&last={l}"));
        }
    }
    Ok((items, next))
}

fn json_list(body: Vec<u8>, path: &str, next: Option<String>) -> Response {
    let mut resp = Response::new(200).bytes(JSON, body);
    if let Some(q) = next {
        resp = resp.header("Link", format!("<{path}?{q}>; rel=\"next\""));
    }
    resp
}

fn tags_list(store: &Store, req: &Request, name: &str) -> Result<Response, ApiError> {
    if !store.repo_exists(name)? {
        return Err(err(
            404,
            "NAME_UNKNOWN",
            format!("repository {name} not known"),
        ));
    }
    let (tags, next) = paginate(req, store.tags(name)?)?;
    let body = TagList {
        name: name.to_string(),
        tags: Some(tags),
    };
    let body = serde_json::to_vec(&body).expect("tag list serializes");
    Ok(json_list(body, &format!("/v2/{name}/tags/list"), next))
}

fn catalog(store: &Store, req: &Request) -> Result<Response, ApiError> {
    let (repos, next) = paginate(req, store.repos()?)?;
    let body = Catalog {
        repositories: Some(repos),
    };
    let body = serde_json::to_vec(&body).expect("catalog serializes");
    Ok(json_list(body, "/v2/_catalog", next))
}

/// OCI index of the manifests in `name` whose `subject` is `digest`.
fn referrers(store: &Store, req: &Request, name: &str, digest: &str) -> Result<Response, ApiError> {
    let filter = req.query("artifactType");
    let mut list = Vec::new();
    for d in store.manifest_digests(name)? {
        let Some(m) = store.manifest(name, &d)? else {
            continue;
        };
        let v: Value = serde_json::from_slice(&m.bytes).unwrap_or_default();
        if v["subject"]["digest"].as_str() != Some(digest) {
            continue;
        }
        let artifact_type = v["artifactType"]
            .as_str()
            .or(v["config"]["mediaType"].as_str());
        if filter.is_some_and(|f| artifact_type != Some(f)) {
            continue;
        }
        let mut desc = json!({
            "mediaType": m.content_type,
            "digest": d,
            "size": m.bytes.len(),
        });
        if let Some(a) = artifact_type {
            desc["artifactType"] = a.into();
        }
        if v["annotations"].is_object() {
            desc["annotations"] = v["annotations"].clone();
        }
        list.push(desc);
    }
    let index = json!({
        "schemaVersion": 2,
        "mediaType": media::OCI_INDEX,
        "manifests": list,
    });
    let body = serde_json::to_vec(&index).expect("index serializes");
    let mut resp = Response::new(200).bytes(media::OCI_INDEX, body);
    if filter.is_some() {
        resp = resp.header("OCI-Filters-Applied", "artifactType");
    }
    Ok(resp)
}

#[cfg(test)]
#[path = "routes_tests.rs"]
mod tests;
