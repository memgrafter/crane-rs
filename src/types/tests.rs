//! crane.ts is the canonical type file; this checks that the JSON field names of the
//! Rust serde types still match it, so the two cannot drift apart silently.
//!
//! Rust names come from serde itself (the `fields` list a derived `Deserialize` passes
//! to `deserialize_struct`, after renames). crane.ts names come from a small reader of
//! `export interface` bodies, including inline object types (`a.b` paths step into
//! `{ ... }`, `{ ... }[]` and `Record<string, { ... }>`).
//!
//! Same:   a full type; Rust and crane.ts must have the same field names.
//! Within: a parsed view that reads only some fields; every Rust field must exist in
//!         crane.ts (a renamed or new Rust field fails until crane.ts is updated).
//! Not checked: types::DockerManifest (serialize-only, borrowed); its bytes are pinned
//! by types::tests::docker_manifest_matches_go_marshal.

use std::collections::{BTreeMap, BTreeSet};

use serde::de::{self, Deserialize, Deserializer, Visitor};

const CRANE_TS: &str = include_str!("../../crane.ts");

// ---- Rust side: field names as serde sees them

struct Capture<'a>(&'a mut Option<&'static [&'static str]>);

impl<'de> Deserializer<'de> for Capture<'_> {
    type Error = de::value::Error;

    fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
        Err(de::Error::custom("not a struct"))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        fields: &'static [&'static str],
        _: V,
    ) -> Result<V::Value, Self::Error> {
        *self.0 = Some(fields);
        Err(de::Error::custom("captured"))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes
        byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map
        enum identifier ignored_any
    }
}

fn rust_fields<T: for<'de> Deserialize<'de>>() -> BTreeSet<String> {
    let mut f = None;
    let _ = T::deserialize(Capture(&mut f));
    f.expect("a derived struct Deserialize")
        .iter()
        .map(|s| s.to_string())
        .collect()
}

// ---- crane.ts side

/// The source with comments removed (string literals kept as they are).
fn strip_comments(src: &str) -> String {
    let b = src.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"') => {
                let end = i + 1 + b[i + 1..].iter().position(|&c| c == q).unwrap();
                out.extend_from_slice(&b[i..=end]);
                i = end + 1;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                i += b[i..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .unwrap_or(b.len() - i);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2 + src[i + 2..].find("*/").unwrap() + 2;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap()
}

/// Text between the first `{` in `s` and its matching `}`.
fn braced(s: &str) -> &str {
    let open = s
        .find('{')
        .unwrap_or_else(|| panic!("no object type in {s:?}"));
    let mut depth = 0;
    for (i, c) in s[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &s[open + 1..open + i];
                }
            }
            _ => {}
        }
    }
    panic!("unclosed {{ in {s:?}")
}

/// Members of an object type body: field name -> type text.
fn members(body: &str) -> BTreeMap<String, String> {
    let mut parts = Vec::new();
    let (mut depth, mut start, mut quote) = (0i32, 0, None);
    for (i, c) in body.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '{' | '[' | '(' | '<') => depth += 1,
            (None, '}' | ']' | ')' | '>') => depth -= 1,
            (None, '\n' | ';' | ',') if depth == 0 => {
                parts.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&body[start..]);
    let mut out = BTreeMap::new();
    for p in parts.into_iter().map(str::trim).filter(|p| !p.is_empty()) {
        let (name, rest) = match p.strip_prefix('\'') {
            Some(q) => q.split_once('\'').unwrap(),
            None => p.split_at(
                p.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                    .unwrap_or(p.len()),
            ),
        };
        let rest = rest.trim_start();
        let ty = rest
            .strip_prefix("?:")
            .or_else(|| rest.strip_prefix(':'))
            .unwrap_or_else(|| panic!("not a `name: type` member: {p:?}"));
        out.insert(name.to_string(), ty.trim().to_string());
    }
    out
}

/// Field names at `path`: "Iface" or "Iface.field.field" (into inline object types).
fn ts_fields(ts: &str, path: &str) -> BTreeSet<String> {
    let mut steps = path.split('.');
    let iface = steps.next().unwrap();
    let head = format!("export interface {iface} {{");
    let at = ts
        .find(&head)
        .unwrap_or_else(|| panic!("crane.ts has no `{head}`"));
    let mut m = members(braced(&ts[at..]));
    for step in steps {
        let ty = m
            .get(step)
            .unwrap_or_else(|| panic!("crane.ts {path}: no field {step}"));
        m = members(braced(ty));
    }
    m.into_keys().collect()
}

enum Mode {
    Same,
    Within,
}

/// Mismatch descriptions for one Rust type against the union of crane.ts paths.
fn compare(
    ts: &str,
    rust_name: &str,
    rust: &BTreeSet<String>,
    paths: &[&str],
    mode: Mode,
) -> Vec<String> {
    let want: BTreeSet<String> = paths.iter().flat_map(|p| ts_fields(ts, p)).collect();
    let mut out = Vec::new();
    let extra: Vec<_> = rust.difference(&want).collect();
    if !extra.is_empty() {
        out.push(format!("{rust_name}: {extra:?} not in crane.ts {paths:?}"));
    }
    let missing: Vec<_> = want.difference(rust).collect();
    if matches!(mode, Mode::Same) && !missing.is_empty() {
        out.push(format!(
            "{rust_name}: crane.ts {paths:?} has {missing:?}, Rust does not"
        ));
    }
    out
}

#[test]
fn rust_types_match_crane_ts() {
    use crate::{auth, local, types};
    let ts = &strip_comments(CRANE_TS);
    let checks: Vec<(&str, BTreeSet<String>, &[&str], Mode)> = vec![
        (
            "types::Platform",
            rust_fields::<types::Platform>(),
            &["Platform"],
            Mode::Same,
        ),
        (
            "types::Descriptor",
            rust_fields::<types::Descriptor>(),
            &["Descriptor"],
            Mode::Same,
        ),
        (
            "types::ManifestView",
            rust_fields::<types::ManifestView>(),
            &["Manifest", "Index"],
            Mode::Within,
        ),
        (
            "types::ImageConfig",
            rust_fields::<types::ImageConfig>(),
            &["ImageConfig"],
            Mode::Within,
        ),
        (
            "types::RootFs",
            rust_fields::<types::RootFs>(),
            &["ImageConfig.rootfs"],
            Mode::Within,
        ),
        (
            "types::TarballManifestEntry",
            rust_fields::<types::TarballManifestEntry>(),
            &["TarballManifestEntry"],
            Mode::Same,
        ),
        (
            "types::ErrorResponse",
            rust_fields::<types::ErrorResponse>(),
            &["ErrorResponse"],
            Mode::Same,
        ),
        (
            "types::ErrorItem",
            rust_fields::<types::ErrorItem>(),
            &["ErrorResponse.errors"],
            Mode::Same,
        ),
        (
            "types::TagList",
            rust_fields::<types::TagList>(),
            &["TagList"],
            Mode::Same,
        ),
        (
            "types::Catalog",
            rust_fields::<types::Catalog>(),
            &["Catalog"],
            Mode::Same,
        ),
        (
            "local::IndexFile",
            rust_fields::<local::IndexFile>(),
            &["Index"],
            Mode::Within,
        ),
        (
            "auth::DockerConfig",
            rust_fields::<auth::DockerConfig>(),
            &["DockerConfig"],
            Mode::Within,
        ),
        (
            "auth::AuthEntry",
            rust_fields::<auth::AuthEntry>(),
            &["DockerConfig.auths"],
            Mode::Within,
        ),
        (
            "auth::TokenResponse",
            rust_fields::<auth::TokenResponse>(),
            &["TokenResponse"],
            Mode::Within,
        ),
    ];
    let errors: Vec<String> = checks
        .into_iter()
        .flat_map(|(name, rust, paths, mode)| compare(ts, name, &rust, paths, mode))
        .collect();
    assert!(
        errors.is_empty(),
        "types drifted from crane.ts:\n{}",
        errors.join("\n")
    );
}

#[test]
fn reader_handles_crane_ts_shapes_and_reports_drift() {
    let ts = strip_comments(
        "/** a: b */\nexport interface A {\n  x: string // y: z\n  'o.v'?: number\n  \
         n?: { p: 'q;r'; s: T[] }[]\n  r: Record<string, { t?: 1, u: 2 }>\n}\n",
    );
    let set = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    assert_eq!(ts_fields(&ts, "A"), set(&["x", "o.v", "n", "r"]));
    assert_eq!(ts_fields(&ts, "A.n"), set(&["p", "s"]));
    assert_eq!(ts_fields(&ts, "A.r"), set(&["t", "u"]));
    // Same reports both directions; Within only Rust fields crane.ts lacks.
    let rust = set(&["x", "extra"]);
    assert_eq!(compare(&ts, "R", &rust, &["A"], Mode::Same).len(), 2);
    assert_eq!(compare(&ts, "R", &rust, &["A"], Mode::Within).len(), 1);
    assert!(compare(&ts, "R", &set(&["x"]), &["A"], Mode::Within).is_empty());
}
