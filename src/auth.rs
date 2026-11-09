//! Credentials and auth challenges.
//!
//! Credentials: `auths` in $DOCKER_CONFIG/config.json (default ~/.docker/config.json).
//! credsStore / credHelpers are not supported in v1.
//! Challenges: `WWW-Authenticate: Bearer realm="..",service="..",scope=".."` or `Basic realm=".."`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl Credentials {
    /// "Basic base64(user:pass)"
    pub fn basic_header(&self) -> String {
        format!(
            "Basic {}",
            base64_encode(format!("{}:{}", self.username, self.password).as_bytes())
        )
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct DockerConfig {
    #[serde(default)]
    auths: BTreeMap<String, AuthEntry>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct AuthEntry {
    #[serde(default)]
    auth: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

/// Where docker config lives: $DOCKER_CONFIG/config.json, else $HOME/.docker/config.json.
pub fn config_path(docker_config: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    match (docker_config, home) {
        (Some(d), _) => Some(Path::new(d).join("config.json")),
        (None, Some(h)) => Some(Path::new(h).join(".docker").join("config.json")),
        (None, None) => None,
    }
}

/// Logins read from a docker config file. A missing file means no logins.
#[derive(Debug, Default)]
pub struct Keychain {
    auths: BTreeMap<String, Credentials>,
}

impl Keychain {
    pub fn from_env() -> Result<Self> {
        Self::from_vars(
            std::env::var("DOCKER_CONFIG").ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        )
    }

    /// Logins from $DOCKER_CONFIG / $HOME values; no config file means no logins.
    pub fn from_vars(docker_config: Option<&str>, home: Option<&str>) -> Result<Self> {
        match config_path(docker_config, home) {
            Some(p) if p.exists() => Self::load(&p),
            _ => Ok(Self::default()),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn parse(json: &[u8]) -> Result<Self> {
        let cfg: DockerConfig = serde_json::from_slice(json)?;
        let mut auths = BTreeMap::new();
        for (key, e) in cfg.auths {
            let creds = if !e.auth.is_empty() {
                let raw = base64_decode(&e.auth).with_context(|| format!("auth for {key}"))?;
                let raw = String::from_utf8(raw).with_context(|| format!("auth for {key}"))?;
                let (u, p) = raw
                    .split_once(':')
                    .with_context(|| format!("auth for {key}: no ':'"))?;
                Credentials {
                    username: u.to_string(),
                    password: p.to_string(),
                }
            } else if !e.username.is_empty() {
                Credentials {
                    username: e.username,
                    password: e.password,
                }
            } else {
                continue;
            };
            auths.insert(normalize_key(&key), creds);
        }
        Ok(Self { auths })
    }

    /// Login for a registry host ("index.docker.io" also matches docker's legacy keys).
    pub fn get(&self, registry: &str) -> Option<&Credentials> {
        self.auths.get(&normalize_key(registry))
    }
}

/// "https://index.docker.io/v1/" -> "index.docker.io"; "http://host:5000/" -> "host:5000".
fn normalize_key(key: &str) -> String {
    let k = key
        .strip_prefix("https://")
        .or_else(|| key.strip_prefix("http://"))
        .unwrap_or(key);
    let k = k.split('/').next().unwrap_or_default();
    match k {
        "docker.io" | "registry-1.docker.io" => "index.docker.io".to_string(),
        other => other.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Challenge {
    /// Registry answered /v2/ with 200: no auth.
    Anonymous,
    Basic,
    Bearer {
        realm: String,
        service: Option<String>,
        scope: Option<String>,
    },
}

impl Challenge {
    /// Parse a WWW-Authenticate header value.
    pub fn parse(header: &str) -> Result<Self> {
        let header = header.trim();
        let (scheme, rest) = header.split_once(' ').unwrap_or((header, ""));
        match scheme.to_ascii_lowercase().as_str() {
            "basic" => Ok(Challenge::Basic),
            "bearer" => {
                let params = parse_params(rest);
                let realm = params
                    .get("realm")
                    .cloned()
                    .context("bearer challenge without realm")?;
                Ok(Challenge::Bearer {
                    realm,
                    service: params.get("service").cloned(),
                    scope: params.get("scope").cloned(),
                })
            }
            _ => bail!("unsupported auth challenge: {header}"),
        }
    }
}

/// key=value or key="quoted, value" pairs separated by commas.
fn parse_params(s: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.next_if(|c| *c == ',' || c.is_whitespace()).is_some() {}
        let key: String =
            std::iter::from_fn(|| chars.next_if(|c| *c != '=' && *c != ',')).collect();
        if key.is_empty() {
            break;
        }
        let mut value = String::new();
        if chars.next_if_eq(&'=').is_some() {
            if chars.next_if_eq(&'"').is_some() {
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => value.extend(chars.next()),
                        c => value.push(c),
                    }
                }
            } else {
                value = std::iter::from_fn(|| chars.next_if(|c| *c != ',')).collect();
                value = value.trim().to_string();
            }
        }
        out.insert(key.trim().to_ascii_lowercase(), value);
    }
    out
}

/// Token endpoint response.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub access_token: String,
}

impl TokenResponse {
    pub fn into_token(self) -> Result<String> {
        match (self.token, self.access_token) {
            (t, _) if !t.is_empty() => Ok(t),
            (_, a) if !a.is_empty() => Ok(a),
            _ => bail!("token response has neither token nor access_token"),
        }
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn base64_decode(input: &str) -> Result<Vec<u8>> {
    let clean: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let trimmed: &[u8] = {
        let end = clean.iter().rposition(|b| *b != b'=').map_or(0, |i| i + 1);
        &clean[..end]
    };
    let mut out = Vec::with_capacity(trimmed.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for &c in trimmed {
        let v = B64
            .iter()
            .position(|x| *x == c)
            .with_context(|| format!("invalid base64 byte {c:#x}"))?;
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for (raw, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("user:pa/ss?", "dXNlcjpwYS9zcz8="),
        ] {
            assert_eq!(base64_encode(raw.as_bytes()), enc);
            assert_eq!(base64_decode(enc).unwrap(), raw.as_bytes());
        }
        assert_eq!(base64_decode("Zm9v\nYmFy").unwrap(), b"foobar");
        assert!(base64_decode("Zm9v!").is_err());
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(base64_decode(&base64_encode(&all)).unwrap(), all);
    }

    #[test]
    fn keychain_parsing() {
        let json = br#"{"auths":{
            "https://index.docker.io/v1/":{"auth":"dXNlcjpwYXNz"},
            "localhost:5050":{"username":"a","password":"b"},
            "http://reg.io:443/":{"auth":"eDp5OnoK"},
            "empty.io":{}
        }}"#;
        let k = Keychain::parse(json).unwrap();
        let hub = k.get("index.docker.io").unwrap();
        assert_eq!(
            (hub.username.as_str(), hub.password.as_str()),
            ("user", "pass")
        );
        assert_eq!(k.get("docker.io"), Some(hub));
        assert_eq!(k.get("registry-1.docker.io"), Some(hub));
        assert_eq!(
            k.get("localhost:5050").unwrap().basic_header(),
            "Basic YTpi"
        );
        // password may contain ':'
        assert_eq!(k.get("reg.io:443").unwrap().password, "y:z\n");
        assert!(k.get("empty.io").is_none());
        assert!(k.get("other.io").is_none());
        assert!(Keychain::parse(b"{}").unwrap().get("x").is_none());
    }

    #[test]
    fn keychain_errors() {
        assert!(Keychain::parse(b"nope").is_err());
        assert!(Keychain::parse(br#"{"auths":{"a":{"auth":"!!"}}}"#).is_err());
        // "noseparator" has no ':'
        assert!(Keychain::parse(br#"{"auths":{"a":{"auth":"bm9zZXBhcmF0b3I="}}}"#).is_err());
        // invalid utf-8 (0xff)
        assert!(Keychain::parse(br#"{"auths":{"a":{"auth":"/w=="}}}"#).is_err());
    }

    #[test]
    fn keychain_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, br#"{"auths":{"h":{"username":"u","password":"p"}}}"#).unwrap();
        assert_eq!(Keychain::load(&p).unwrap().get("h").unwrap().username, "u");
        assert!(Keychain::load(&dir.path().join("missing.json")).is_err());
        std::fs::write(&p, b"bad").unwrap();
        assert!(Keychain::load(&p).is_err());
    }

    #[test]
    fn config_paths() {
        assert_eq!(
            config_path(Some("/d"), Some("/h")),
            Some(PathBuf::from("/d/config.json"))
        );
        assert_eq!(
            config_path(None, Some("/h")),
            Some(PathBuf::from("/h/.docker/config.json"))
        );
        assert_eq!(config_path(None, None), None);
    }

    #[test]
    fn keychain_from_vars() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        assert!(
            Keychain::from_vars(Some(d), None)
                .unwrap()
                .get("h")
                .is_none()
        );
        assert!(Keychain::from_vars(None, None).unwrap().get("h").is_none());
        std::fs::write(
            dir.path().join("config.json"),
            br#"{"auths":{"h":{"username":"u","password":"p"}}}"#,
        )
        .unwrap();
        assert_eq!(
            Keychain::from_vars(Some(d), Some("/nonexistent"))
                .unwrap()
                .get("h")
                .unwrap()
                .username,
            "u"
        );
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".docker")).unwrap();
        std::fs::write(home.path().join(".docker/config.json"), b"{}").unwrap();
        assert!(
            Keychain::from_vars(None, home.path().to_str())
                .unwrap()
                .get("h")
                .is_none()
        );
    }

    #[test]
    fn challenges() {
        assert_eq!(
            Challenge::parse("Basic realm=\"x\"").unwrap(),
            Challenge::Basic
        );
        assert_eq!(Challenge::parse("basic").unwrap(), Challenge::Basic);
        assert_eq!(
            Challenge::parse(r#"Bearer realm="https://auth.io/token",service="reg.io",scope="repository:a/b:pull,push""#)
                .unwrap(),
            Challenge::Bearer {
                realm: "https://auth.io/token".into(),
                service: Some("reg.io".into()),
                scope: Some("repository:a/b:pull,push".into()),
            }
        );
        assert_eq!(
            Challenge::parse(r#"Bearer realm=https://a/t , Service="s\"q""#).unwrap(),
            Challenge::Bearer {
                realm: "https://a/t".into(),
                service: Some("s\"q".into()),
                scope: None
            }
        );
        assert!(Challenge::parse("Bearer service=x").is_err());
        assert!(Challenge::parse("Negotiate abc").is_err());
        assert_eq!(
            parse_params("flag, k=v"),
            BTreeMap::from([("flag".into(), String::new()), ("k".into(), "v".into())])
        );
    }

    #[test]
    fn token_responses() {
        let t: TokenResponse = serde_json::from_str(r#"{"token":"a","access_token":"b"}"#).unwrap();
        assert_eq!(t.into_token().unwrap(), "a");
        let t: TokenResponse = serde_json::from_str(r#"{"access_token":"b"}"#).unwrap();
        assert_eq!(t.into_token().unwrap(), "b");
        let t: TokenResponse = serde_json::from_str(r#"{}"#).unwrap();
        assert!(t.into_token().is_err());
    }
}
