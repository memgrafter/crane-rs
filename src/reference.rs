//! Image references, parsed the way go-containerregistry's `name` package does
//! (non-strict mode), so both cranes accept and print the same names.
//!
//!   [registry[:port]/]repository[:tag][@digest]
//!
//! - The first path part is the registry if it is "localhost" or contains '.' or ':'.
//!   Otherwise the registry is Docker Hub ("index.docker.io"; "docker.io" is rewritten to it).
//! - Docker Hub repositories with no '/' get "library/".
//! - Tag defaults to "latest" when there is no digest.

use anyhow::{Result, bail};

use crate::digest;

pub const DEFAULT_REGISTRY: &str = "index.docker.io";
pub const DEFAULT_TAG: &str = "latest";
const REPOSITORY_CHARS: &str = "abcdefghijklmnopqrstuvwxyz0123456789_-./";
const TAG_CHARS: &str = "abcdefghijklmnopqrstuvwxyz0123456789_-.ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// registry + repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    pub registry: String,
    pub repository: String,
}

/// A repository plus a tag or a digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub repo: Repository,
    pub kind: RefKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefKind {
    Tag(String),
    Digest(String),
}

fn check_element(what: &str, value: &str, allowed: &str, min: usize, max: usize) -> Result<()> {
    let n = value.chars().count();
    if n < min || n > max {
        bail!("{what} must be between {min} and {max} characters in length: {value}");
    }
    if value.chars().any(|c| !allowed.contains(c)) {
        bail!("{what} can only contain the characters `{allowed}`: {value}");
    }
    Ok(())
}

/// Registry names must be a plain URI authority: host[:port], no path, user info or spaces.
fn check_registry(name: &str) -> Result<()> {
    let bad = name
        .chars()
        .any(|c| matches!(c, '/' | '@' | '?' | '#' | ' ' | '\\') || c.is_control());
    if bad {
        bail!("registries must be valid RFC 3986 URI authorities: {name}");
    }
    Ok(())
}

impl Repository {
    pub fn parse(name: &str) -> Result<Self> {
        if name.is_empty() {
            bail!("a repository name must be specified");
        }
        let (registry, repo) = match name.split_once('/') {
            Some((first, rest)) if first == "localhost" || first.contains(['.', ':']) => {
                (first, rest)
            }
            _ => ("", name),
        };
        check_element("repository", repo, REPOSITORY_CHARS, 1, 255)?;
        check_registry(registry)?;
        let registry = match registry {
            "" | "docker.io" => DEFAULT_REGISTRY,
            r => r,
        };
        let repository = if registry == DEFAULT_REGISTRY && !repo.contains('/') {
            format!("library/{repo}")
        } else {
            repo.to_string()
        };
        Ok(Self {
            registry: registry.to_string(),
            repository,
        })
    }

    /// "registry/repository"
    pub fn name(&self) -> String {
        format!("{}/{}", self.registry, self.repository)
    }

    /// Token scope, e.g. "repository:library/alpine:pull".
    pub fn scope(&self, actions: &str) -> String {
        format!("repository:{}:{actions}", self.repository)
    }

    pub fn tag(&self, tag: &str) -> Reference {
        Reference {
            repo: self.clone(),
            kind: RefKind::Tag(tag.to_string()),
        }
    }

    pub fn digest(&self, digest: &str) -> Reference {
        Reference {
            repo: self.clone(),
            kind: RefKind::Digest(digest.to_string()),
        }
    }
}

impl Reference {
    /// go `name.ParseReference`: a tag reference first, then a digest reference.
    pub fn parse(s: &str) -> Result<Self> {
        match Self::parse_tag(s) {
            Ok(r) => Ok(r),
            Err(tag_err) => {
                if s.contains('@') {
                    Self::parse_digest(s)
                } else {
                    Err(tag_err)
                }
            }
        }
    }

    fn split_tag(s: &str) -> (&str, Option<&str>) {
        match s.rsplit_once(':') {
            Some((base, tag)) if !tag.contains('/') => (base, Some(tag)),
            _ => (s, None),
        }
    }

    fn parse_tag(s: &str) -> Result<Self> {
        let (base, tag) = Self::split_tag(s);
        let tag = match tag {
            Some(t) => {
                check_element("tag", t, TAG_CHARS, 1, 128)?;
                t
            }
            None => DEFAULT_TAG,
        };
        Ok(Repository::parse(base)?.tag(tag))
    }

    fn parse_digest(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split('@').collect();
        if parts.len() != 2 {
            bail!(
                "a digest must contain exactly one '@' separator (e.g. registry/repository@digest) saw: {s}"
            );
        }
        let (base, dig) = (parts[0], parts[1]);
        if !digest::is_valid(dig) {
            bail!("invalid digest (only sha256 is supported): {dig}");
        }
        // A tag next to the digest is allowed and ignored.
        let base = match Self::parse_tag(base) {
            Ok(r) => r.repo.name(),
            Err(_) => base.to_string(),
        };
        Ok(Repository::parse(&base)?.digest(dig))
    }

    /// The tag or digest, as used in /v2/<name>/manifests/<identifier>.
    pub fn identifier(&self) -> &str {
        match &self.kind {
            RefKind::Tag(t) | RefKind::Digest(t) => t,
        }
    }

    pub fn is_digest(&self) -> bool {
        matches!(self.kind, RefKind::Digest(_))
    }

    pub fn registry(&self) -> &str {
        &self.repo.registry
    }
}

impl std::fmt::Display for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            RefKind::Tag(t) => write!(f, "{}:{t}", self.repo.name()),
            RefKind::Digest(d) => write!(f, "{}@{d}", self.repo.name()),
        }
    }
}

/// Validates a registry name given on its own (`crane catalog <registry>`).
pub fn parse_registry(name: &str) -> Result<String> {
    check_registry(name)?;
    Ok(match name {
        "" | "docker.io" => DEFAULT_REGISTRY.to_string(),
        r => r.to_string(),
    })
}

/// URL schemes to try for a registry, in order (go `Registry.Scheme` + ping fallback):
/// https only, or https then http for insecure, RFC 1918, localhost and loopback registries.
pub fn schemes(registry: &str, insecure: bool) -> &'static [&'static str] {
    if insecure || allows_http(registry) {
        &["https", "http"]
    } else {
        &["https"]
    }
}

fn allows_http(registry: &str) -> bool {
    let host = registry.split(':').next().unwrap_or_default();
    if host
        .parse::<std::net::Ipv4Addr>()
        .is_ok_and(|ip| ip.is_private())
    {
        return true;
    }
    let port_ok = |rest: &str| {
        rest.is_empty()
            || rest.strip_prefix(':').is_some_and(|p| {
                (1..=5).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit())
            })
    };
    registry.starts_with("localhost:")
        || registry
            .rfind(".localhost")
            .is_some_and(|i| port_ok(&registry[i + ".localhost".len()..]))
        || registry.strip_prefix("127.0.0.1").is_some_and(port_ok)
        || registry == "::1"
        || registry.strip_prefix("[::1]").is_some_and(port_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn p(s: &str) -> String {
        Reference::parse(s).unwrap().to_string()
    }

    #[test]
    fn docker_hub_defaults() {
        assert_eq!(p("alpine"), "index.docker.io/library/alpine:latest");
        assert_eq!(p("docker.io/alpine:3"), "index.docker.io/library/alpine:3");
        assert_eq!(
            p("index.docker.io/alpine"),
            "index.docker.io/library/alpine:latest"
        );
        assert_eq!(p("user/app:v1"), "index.docker.io/user/app:v1");
    }

    #[test]
    fn registries_and_ports() {
        assert_eq!(p("localhost/app"), "localhost/app:latest");
        assert_eq!(p("localhost:5050/a/b:t"), "localhost:5050/a/b:t");
        assert_eq!(
            p("gcr.io/distroless/static:nonroot"),
            "gcr.io/distroless/static:nonroot"
        );
        assert_eq!(p("10.0.0.5:5000/app:v12"), "10.0.0.5:5000/app:v12");
        // a port-looking suffix after the last '/' is a tag, before it is a registry port
        assert_eq!(p("host:5000/app"), "host:5000/app:latest");
    }

    #[test]
    fn digests() {
        let r = Reference::parse(&format!("gcr.io/x/y@{D}")).unwrap();
        assert!(r.is_digest());
        assert_eq!(r.identifier(), D);
        assert_eq!(r.registry(), "gcr.io");
        assert_eq!(r.to_string(), format!("gcr.io/x/y@{D}"));
        // tag + digest: the tag is dropped
        assert_eq!(p(&format!("gcr.io/x/y:t@{D}")), format!("gcr.io/x/y@{D}"));
        assert_eq!(
            p(&format!("alpine@{D}")),
            format!("index.docker.io/library/alpine@{D}")
        );
    }

    #[test]
    fn rejects_bad_names() {
        for bad in [
            "",
            "UPPER/case",
            "a/b:bad tag",
            "a/b:",
            &format!("a/b:{}", "t".repeat(129)),
            "a/b@sha256:short",
            &format!("a@{D}@{D}"),
            &format!("a/b:bad!@{D}"),
            "bad host.io/x",
            "gcr.io/",
            &"a".repeat(256),
        ] {
            assert!(Reference::parse(bad).is_err(), "accepted {bad:?}");
        }
        // a bad tag before a valid digest falls back to parsing the base as a repository
        assert!(Reference::parse(&format!("A@{D}")).is_err());
    }

    #[test]
    fn length_limits_are_inclusive() {
        assert!(Reference::parse(&format!("a/b:{}", "t".repeat(128))).is_ok());
        assert!(Repository::parse(&"a".repeat(255)).is_ok());
    }

    #[test]
    fn identifier_tag() {
        let r = Reference::parse("a/b:v1").unwrap();
        assert_eq!(r.identifier(), "v1");
        assert!(!r.is_digest());
    }

    #[test]
    fn repository_helpers() {
        let repo = Repository::parse("gcr.io/a/b").unwrap();
        assert_eq!(repo.name(), "gcr.io/a/b");
        assert_eq!(repo.scope("pull,push"), "repository:a/b:pull,push");
        assert_eq!(repo.tag("t").to_string(), "gcr.io/a/b:t");
        assert_eq!(repo.digest(D).to_string(), format!("gcr.io/a/b@{D}"));
        assert!(Repository::parse("").is_err());
    }

    #[test]
    fn registry_only() {
        assert_eq!(parse_registry("docker.io").unwrap(), DEFAULT_REGISTRY);
        assert_eq!(parse_registry("").unwrap(), DEFAULT_REGISTRY);
        assert_eq!(parse_registry("localhost:5050").unwrap(), "localhost:5050");
        assert!(parse_registry("a/b").is_err());
        assert!(parse_registry("a\u{7}").is_err());
    }

    #[test]
    fn scheme_choice() {
        let both: &[&str] = &["https", "http"];
        let tls: &[&str] = &["https"];
        assert_eq!(schemes("gcr.io", false), tls);
        assert_eq!(schemes("gcr.io", true), both);
        for r in [
            "192.168.0.10:5000",
            "10.1.2.3",
            "172.16.0.1:80",
            "localhost:5050",
            "127.0.0.1",
            "127.0.0.1:5050",
            "::1",
            "[::1]:5050",
            "reg.localhost",
            "reg.localhost:5050",
        ] {
            assert_eq!(schemes(r, false), both, "{r}");
        }
        for r in [
            "8.8.8.8",
            "172.32.0.1",
            "localhost",
            "127.0.0.1:123456",
            "127.0.0.1:x",
            "reg.localhost.example.com",
            "[::1]:",
            "127.0.0.2",
        ] {
            assert_eq!(schemes(r, false), tls, "{r}");
        }
    }
}
