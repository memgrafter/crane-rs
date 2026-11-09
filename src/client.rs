//! Registry HTTP client (OCI Distribution v2) on ureq: ping and auth, manifests,
//! blobs, uploads, tag and catalog listing with pagination.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use ureq::http::{self, Response};
use ureq::{Body, SendBody};

use crate::auth::{Challenge, Keychain, TokenResponse};
use crate::digest;
use crate::reference::{self, Reference, Repository};
use crate::types::{Catalog, ErrorResponse, TagList, media};

pub type Resp = Response<Body>;

/// A manifest as fetched: original bytes, Content-Type and the digest of the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub bytes: Vec<u8>,
    pub content_type: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub digest: String,
    pub content_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint {
    base: String,
    challenge: Challenge,
}

/// Result of a cross-repo mount attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    Mounted,
    /// The registry started a normal upload instead; continue at this URL.
    Upload(String),
}

pub struct Client {
    agent: ureq::Agent,
    insecure: bool,
    verbose: bool,
    keychain: Keychain,
    endpoints: RefCell<HashMap<String, Endpoint>>,
    tokens: RefCell<HashMap<String, String>>,
}

const USER_AGENT: &str = concat!("crane-rs/", env!("CARGO_PKG_VERSION"));

impl Client {
    pub fn new(insecure: bool, verbose: bool, keychain: Keychain) -> Self {
        let tls = ureq::tls::TlsConfig::builder()
            .disable_verification(insecure)
            .build();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .tls_config(tls)
            .timeout_connect(Some(Duration::from_secs(30)))
            .user_agent(USER_AGENT)
            .max_redirects(10)
            .build()
            .into();
        Self {
            agent,
            insecure,
            verbose,
            keychain,
            endpoints: RefCell::default(),
            tokens: RefCell::default(),
        }
    }

    fn log(&self, line: impl FnOnce() -> String) {
        if self.verbose {
            eprintln!("{}", line());
        }
    }

    /// GET /v2/ over each allowed scheme; the first one that answers wins.
    fn endpoint(&self, registry: &str) -> Result<Endpoint> {
        if let Some(e) = self.endpoints.borrow().get(registry) {
            return Ok(e.clone());
        }
        let mut last_err = None;
        for scheme in reference::schemes(registry, self.insecure) {
            let base = format!("{scheme}://{registry}");
            let url = format!("{base}/v2/");
            match self.agent.get(&url).call() {
                Err(e) => {
                    self.log(|| format!("ping {url}: {e}"));
                    last_err = Some(anyhow!(e).context(format!("GET {url}")));
                }
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    self.log(|| format!("GET {url} -> {status}"));
                    let challenge = match status {
                        200 => Challenge::Anonymous,
                        401 => Challenge::parse(&header(&resp, "www-authenticate").with_context(
                            || format!("GET {url}: 401 without WWW-Authenticate"),
                        )?)?,
                        _ => return Err(status_error(resp, "GET", &url)),
                    };
                    let e = Endpoint { base, challenge };
                    self.endpoints
                        .borrow_mut()
                        .insert(registry.to_string(), e.clone());
                    return Ok(e);
                }
            }
        }
        Err(last_err.expect("at least one scheme"))
    }

    /// The Authorization header for `scopes` on `registry`, if one is needed.
    fn authorization(&self, registry: &str, scopes: &[String]) -> Result<Option<String>> {
        let ep = self.endpoint(registry)?;
        let creds = self.keychain.get(registry);
        match ep.challenge {
            Challenge::Anonymous => Ok(None),
            Challenge::Basic => Ok(creds.map(|c| c.basic_header())),
            Challenge::Bearer { realm, service, .. } => {
                let key = format!("{registry}|{}", scopes.join(" "));
                if let Some(t) = self.tokens.borrow().get(&key) {
                    return Ok(Some(format!("Bearer {t}")));
                }
                let mut url = realm.clone();
                let mut sep = if realm.contains('?') { '&' } else { '?' };
                for (k, v) in service
                    .iter()
                    .map(|s| ("service", s))
                    .chain(scopes.iter().map(|s| ("scope", s)))
                {
                    url.push(sep);
                    url.push_str(k);
                    url.push('=');
                    url.push_str(&percent_encode(v));
                    sep = '&';
                }
                let mut req = self.agent.get(&url);
                if let Some(c) = creds {
                    req = req.header("Authorization", c.basic_header());
                }
                let resp = req.call().with_context(|| format!("GET {url}"))?;
                self.log(|| format!("GET {url} -> {}", resp.status()));
                let resp = check(resp, "GET", &url, &[200])?;
                let token: TokenResponse = serde_json::from_slice(&read_all(resp)?)
                    .with_context(|| format!("GET {url}: bad token response"))?;
                let token = token.into_token()?;
                self.tokens.borrow_mut().insert(key, token.clone());
                Ok(Some(format!("Bearer {token}")))
            }
        }
    }

    /// Send one request with auth. `path` starts with "/v2/" or is an absolute URL.
    fn send(
        &self,
        registry: &str,
        scopes: &[String],
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: SendBody<'_>,
    ) -> Result<(Resp, String)> {
        let ep = self.endpoint(registry)?;
        let url = if path.starts_with('/') {
            format!("{}{path}", ep.base)
        } else {
            path.to_string()
        };
        let mut b = http::Request::builder().method(method).uri(&url);
        for (k, v) in headers {
            b = b.header(*k, v);
        }
        if let Some(a) = self.authorization(registry, scopes)? {
            b = b.header("Authorization", a);
        }
        let req = b.body(body).with_context(|| format!("{method} {url}"))?;
        let resp = self
            .agent
            .run(req)
            .with_context(|| format!("{method} {url}"))?;
        self.log(|| format!("{method} {url} -> {}", resp.status()));
        Ok((resp, url))
    }

    fn send_empty(
        &self,
        repo: &Repository,
        actions: &str,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
    ) -> Result<(Resp, String)> {
        self.send(
            &repo.registry,
            &[repo.scope(actions)],
            method,
            path,
            headers,
            SendBody::none(),
        )
    }

    pub fn get_manifest(&self, r: &Reference) -> Result<Fetched> {
        let path = format!("/v2/{}/manifests/{}", r.repo.repository, r.identifier());
        let (resp, url) = self.send_empty(
            &r.repo,
            "pull",
            "GET",
            &path,
            &[("Accept", media::ACCEPT.into())],
        )?;
        let resp = check(resp, "GET", &url, &[200])?;
        let content_type = header(&resp, "content-type").unwrap_or_default();
        let bytes = read_all(resp)?;
        let digest = digest::of(&bytes);
        if r.is_digest() && digest != r.identifier() {
            bail!(
                "GET {url}: manifest digest {digest} does not match {}",
                r.identifier()
            );
        }
        Ok(Fetched {
            bytes,
            content_type,
            digest,
        })
    }

    /// HEAD the manifest; None if it does not exist.
    pub fn head_manifest(&self, r: &Reference) -> Result<Option<Head>> {
        let path = format!("/v2/{}/manifests/{}", r.repo.repository, r.identifier());
        let (resp, url) = self.send_empty(
            &r.repo,
            "pull",
            "HEAD",
            &path,
            &[("Accept", media::ACCEPT.into())],
        )?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        let resp = check(resp, "HEAD", &url, &[200])?;
        let digest = header(&resp, "docker-content-digest")
            .filter(|d| digest::is_valid(d))
            .with_context(|| format!("HEAD {url}: no Docker-Content-Digest"))?;
        Ok(Some(Head {
            digest,
            content_type: header(&resp, "content-type").unwrap_or_default(),
            size: header(&resp, "content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        }))
    }

    pub fn put_manifest(
        &self,
        repo: &Repository,
        identifier: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<String> {
        let path = format!("/v2/{}/manifests/{identifier}", repo.repository);
        let headers = [
            ("Content-Type", content_type.to_string()),
            ("Content-Length", bytes.len().to_string()),
        ];
        let (resp, url) = self.send(
            &repo.registry,
            &[repo.scope("pull,push")],
            "PUT",
            &path,
            &headers,
            SendBody::from_owned_reader(std::io::Cursor::new(bytes.to_vec())),
        )?;
        check(resp, "PUT", &url, &[200, 201, 202])?;
        Ok(digest::of(bytes))
    }

    pub fn delete_manifest(&self, r: &Reference) -> Result<()> {
        let path = format!("/v2/{}/manifests/{}", r.repo.repository, r.identifier());
        let (resp, url) = self.send_empty(&r.repo, "pull,push,delete", "DELETE", &path, &[])?;
        check(resp, "DELETE", &url, &[200, 202])?;
        Ok(())
    }

    pub fn blob_exists(&self, repo: &Repository, digest: &str) -> Result<bool> {
        let path = format!("/v2/{}/blobs/{digest}", repo.repository);
        let (resp, url) = self.send_empty(repo, "pull", "HEAD", &path, &[])?;
        match resp.status().as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            _ => Err(status_error(resp, "HEAD", &url)),
        }
    }

    /// Stream a blob. The caller verifies the digest while reading.
    pub fn get_blob(&self, repo: &Repository, digest: &str) -> Result<Box<dyn Read + Send>> {
        let path = format!("/v2/{}/blobs/{digest}", repo.repository);
        let (resp, url) = self.send_empty(repo, "pull", "GET", &path, &[])?;
        let resp = check(resp, "GET", &url, &[200])?;
        Ok(Box::new(resp.into_body().into_reader()))
    }

    /// Try a cross-repo mount from `from` (same registry).
    pub fn mount(&self, repo: &Repository, digest: &str, from: &Repository) -> Result<Mount> {
        let path = format!(
            "/v2/{}/blobs/uploads/?mount={}&from={}",
            repo.repository,
            percent_encode(digest),
            percent_encode(&from.repository)
        );
        let scopes = [repo.scope("pull,push"), from.scope("pull")];
        let (resp, url) = self.send(
            &repo.registry,
            &scopes,
            "POST",
            &path,
            &[("Content-Length", "0".into())],
            SendBody::none(),
        )?;
        match resp.status().as_u16() {
            201 => Ok(Mount::Mounted),
            202 => Ok(Mount::Upload(location(&resp, &url)?)),
            _ => Err(status_error(resp, "POST", &url)),
        }
    }

    /// Upload a blob: POST to start (unless `upload_url` came from a failed mount), then
    /// one PUT with the whole body and `?digest=`.
    pub fn upload_blob(
        &self,
        repo: &Repository,
        digest: &str,
        size: u64,
        body: Box<dyn Read + Send>,
        upload_url: Option<String>,
    ) -> Result<()> {
        let scopes = [repo.scope("pull,push")];
        let start = match upload_url {
            Some(u) => u,
            None => {
                let path = format!("/v2/{}/blobs/uploads/", repo.repository);
                let (resp, url) = self.send(
                    &repo.registry,
                    &scopes,
                    "POST",
                    &path,
                    &[("Content-Length", "0".into())],
                    SendBody::none(),
                )?;
                let resp = check(resp, "POST", &url, &[202])?;
                location(&resp, &url)?
            }
        };
        let sep = if start.contains('?') { '&' } else { '?' };
        let put_url = format!("{start}{sep}digest={}", percent_encode(digest));
        let headers = [
            ("Content-Type", "application/octet-stream".to_string()),
            ("Content-Length", size.to_string()),
        ];
        let (resp, url) = self.send(
            &repo.registry,
            &scopes,
            "PUT",
            &put_url,
            &headers,
            SendBody::from_owned_reader(body),
        )?;
        check(resp, "PUT", &url, &[201, 204])?;
        Ok(())
    }

    /// All tags, following `Link: <...>; rel="next"` pages.
    pub fn tags(&self, repo: &Repository) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut next = Some(format!("/v2/{}/tags/list", repo.repository));
        while let Some(path) = next.take() {
            let (resp, url) = self.send_empty(repo, "pull", "GET", &path, &[])?;
            let resp = check(resp, "GET", &url, &[200])?;
            next = next_link(&resp);
            let list: TagList =
                serde_json::from_slice(&read_all(resp)?).with_context(|| format!("GET {url}"))?;
            out.extend(list.tags.unwrap_or_default());
        }
        Ok(out)
    }

    /// All repositories, following pagination.
    pub fn catalog(&self, registry: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut next = Some("/v2/_catalog".to_string());
        let scopes = ["registry:catalog:*".to_string()];
        while let Some(path) = next.take() {
            let (resp, url) = self.send(registry, &scopes, "GET", &path, &[], SendBody::none())?;
            let resp = check(resp, "GET", &url, &[200])?;
            next = next_link(&resp);
            let cat: Catalog =
                serde_json::from_slice(&read_all(resp)?).with_context(|| format!("GET {url}"))?;
            out.extend(cat.repositories.unwrap_or_default());
        }
        Ok(out)
    }
}

pub fn header(resp: &Resp, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

pub fn read_all(resp: Resp) -> Result<Vec<u8>> {
    Ok(resp
        .into_body()
        .with_config()
        .limit(u64::MAX)
        .read_to_vec()?)
}

/// Error unless the status is one of `ok`.
fn check(resp: Resp, method: &str, url: &str, ok: &[u16]) -> Result<Resp> {
    if ok.contains(&resp.status().as_u16()) {
        Ok(resp)
    } else {
        Err(status_error(resp, method, url))
    }
}

/// "<METHOD> <url>: <status>: <registry error codes or body>"
fn status_error(resp: Resp, method: &str, url: &str) -> anyhow::Error {
    let status = resp.status();
    let body = resp
        .into_body()
        .with_config()
        .limit(64 * 1024)
        .read_to_vec()
        .unwrap_or_default();
    let detail = match serde_json::from_slice::<ErrorResponse>(&body) {
        Ok(e) if !e.errors.is_empty() => e.summary(),
        _ => String::from_utf8_lossy(&body)
            .trim()
            .chars()
            .take(200)
            .collect(),
    };
    if detail.is_empty() {
        anyhow!("{method} {url}: unexpected status code {status}")
    } else {
        anyhow!("{method} {url}: unexpected status code {status}: {detail}")
    }
}

/// Location header resolved against the request URL's origin.
fn location(resp: &Resp, url: &str) -> Result<String> {
    let loc =
        header(resp, "location").with_context(|| format!("{url}: response has no Location"))?;
    Ok(resolve(url, &loc))
}

fn resolve(url: &str, loc: &str) -> String {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.to_string();
    }
    let origin_end = url
        .find("://")
        .map(|i| i + 3)
        .and_then(|s| url[s..].find('/').map(|j| s + j))
        .unwrap_or(url.len());
    format!("{}{loc}", &url[..origin_end])
}

/// Path of the `rel="next"` Link, if any.
fn next_link(resp: &Resp) -> Option<String> {
    let link = header(resp, "link")?;
    link.split(',').find_map(|part| {
        let (target, params) = part.split_once(';')?;
        params.contains("rel=\"next\"").then(|| {
            target
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        })
    })
}

pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests;
