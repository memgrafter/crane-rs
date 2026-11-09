use std::io::Read;

use super::*;
use crate::auth::Keychain;
use crate::testutil::{Res, dead_addr, serve};

const D: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn anon(c: impl Fn(&crate::testutil::Req) -> Res + Send + Sync + 'static) -> crate::testutil::Fake {
    serve(move |r| {
        if r.path == "/v2/" {
            Res::new(200)
        } else {
            c(r)
        }
    })
}

fn client() -> Client {
    Client::new(false, false, Keychain::default())
}

fn repo(addr: &str) -> Repository {
    Repository::parse(&format!("{addr}/a/b")).unwrap()
}

fn keychain(addr: &str) -> Keychain {
    Keychain::parse(
        format!(r#"{{"auths":{{"{addr}":{{"username":"u","password":"p"}}}}}}"#).as_bytes(),
    )
    .unwrap()
}

#[test]
fn ping_is_cached_and_falls_back_to_http() {
    let f = anon(|_| Res::new(200).body(r#"{"name":"a/b","tags":["x"]}"#));
    let c = Client::new(false, true, Keychain::default()); // verbose path
    assert_eq!(c.tags(&repo(&f.addr)).unwrap(), vec!["x"]);
    assert_eq!(c.tags(&repo(&f.addr)).unwrap(), vec!["x"]);
    assert_eq!(
        f.calls()
            .iter()
            .filter(|c| c.as_str() == "GET /v2/")
            .count(),
        1
    );
}

#[test]
fn ping_errors() {
    let c = client();
    // nothing listening: every scheme fails
    let err = c.tags(&repo(&dead_addr())).unwrap_err();
    assert!(format!("{err:#}").contains("/v2/"), "{err:#}");
    // 401 without a challenge
    let f = serve(|_| Res::new(401));
    assert!(
        format!("{:#}", c.tags(&repo(&f.addr)).unwrap_err()).contains("without WWW-Authenticate")
    );
    // unexpected status
    let f = serve(|_| Res::new(500).body("boom"));
    assert!(format!("{:#}", c.tags(&repo(&f.addr)).unwrap_err()).contains("500"));
    // unsupported challenge
    let f = serve(|_| Res::new(401).header("WWW-Authenticate", "Negotiate x"));
    assert!(c.tags(&repo(&f.addr)).is_err());
}

#[test]
fn bearer_flow_fetches_and_caches_tokens() {
    let token_srv = serve(|r| {
        assert_eq!(r.header("authorization"), Some("Basic dTpw"));
        Res::new(200).body(r#"{"access_token":"tok"}"#)
    });
    let realm = format!("http://{}/token?x=1", token_srv.addr);
    let f = serve(move |r| match r.path.as_str() {
        "/v2/" => Res::new(401).header(
            "WWW-Authenticate",
            &format!(r#"Bearer realm="{realm}",service="svc""#),
        ),
        _ => {
            assert_eq!(r.header("authorization"), Some("Bearer tok"));
            Res::new(200).body(r#"{"name":"a/b","tags":null}"#)
        }
    });
    let c = Client::new(false, true, keychain(&f.addr)); // verbose: logs the token request too
    assert!(c.tags(&repo(&f.addr)).unwrap().is_empty());
    assert!(c.tags(&repo(&f.addr)).unwrap().is_empty());
    let calls = token_srv.calls();
    assert_eq!(
        calls,
        vec!["GET /token?x=1&service=svc&scope=repository%3Aa%2Fb%3Apull"]
    );
}

#[test]
fn bearer_without_service_or_creds() {
    let token_srv = serve(|r| {
        assert_eq!(r.header("authorization"), None);
        Res::new(200).body(r#"{"token":"t2"}"#)
    });
    let realm = format!("http://{}/token", token_srv.addr);
    let f = serve(move |r| match r.path.as_str() {
        "/v2/" => Res::new(401).header("WWW-Authenticate", &format!(r#"Bearer realm="{realm}""#)),
        _ => Res::new(200).body(r#"{"repositories":["r"]}"#),
    });
    assert_eq!(client().catalog(&f.addr).unwrap(), vec!["r"]);
    assert_eq!(
        token_srv.calls(),
        vec!["GET /token?scope=registry%3Acatalog%3A%2A"]
    );
}

#[test]
fn bearer_token_errors() {
    for body in [
        Res::new(401),
        Res::new(200).body("not json"),
        Res::new(200).body("{}"),
    ] {
        let token_srv = serve(move |_| body.clone());
        let realm = format!("http://{}/t", token_srv.addr);
        let f = serve(move |_| {
            Res::new(401).header("WWW-Authenticate", &format!(r#"Bearer realm="{realm}""#))
        });
        assert!(client().tags(&repo(&f.addr)).is_err());
    }
    // realm that does not answer
    let realm = format!("http://{}/t", dead_addr());
    let f = serve(move |_| {
        Res::new(401).header("WWW-Authenticate", &format!(r#"Bearer realm="{realm}""#))
    });
    assert!(format!("{:#}", client().tags(&repo(&f.addr)).unwrap_err()).contains("/t"));
}

#[test]
fn basic_auth() {
    let f = serve(|r| match r.path.as_str() {
        "/v2/" => Res::new(401).header("WWW-Authenticate", "Basic realm=\"r\""),
        _ => {
            assert_eq!(r.header("authorization"), Some("Basic dTpw"));
            Res::new(200).body(r#"{"name":"a/b","tags":["t"]}"#)
        }
    });
    let c = Client::new(false, false, keychain(&f.addr));
    assert_eq!(c.tags(&repo(&f.addr)).unwrap(), vec!["t"]);
    // no creds: no header, server decides
    let f2 = serve(|r| match r.path.as_str() {
        "/v2/" => Res::new(401).header("WWW-Authenticate", "Basic"),
        _ => {
            assert_eq!(r.header("authorization"), None);
            Res::new(401).body(r#"{"errors":[{"code":"UNAUTHORIZED","message":"login"}]}"#)
        }
    });
    let err = format!("{:#}", client().tags(&repo(&f2.addr)).unwrap_err());
    assert!(
        err.contains("401") && err.contains("UNAUTHORIZED: login"),
        "{err}"
    );
}

#[test]
fn manifests() {
    let f = anon(|r| match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/v2/a/b/manifests/t") => {
            assert!(r.header("accept").unwrap().contains(media::OCI_INDEX));
            Res::new(200)
                .header("Content-Type", media::OCI_MANIFEST)
                .body("{}")
        }
        ("GET", "/v2/a/b/manifests/nct") => Res::new(200).body("{}"),
        ("GET", p) if p.starts_with("/v2/a/b/manifests/sha256:") => {
            Res::new(200).body("not-matching")
        }
        ("HEAD", "/v2/a/b/manifests/t") => Res::new(200)
            .header("Docker-Content-Digest", D)
            .header("Content-Type", "ct")
            .header("Content-Length", "7"),
        ("HEAD", "/v2/a/b/manifests/nodigest") => Res::new(200),
        ("HEAD", "/v2/a/b/manifests/bare") => Res::new(200).header("Docker-Content-Digest", D),
        ("HEAD", "/v2/a/b/manifests/boom") => Res::new(500),
        ("PUT", "/v2/a/b/manifests/t") => {
            assert_eq!(r.header("content-type"), Some("ct"));
            assert_eq!(r.body, b"{}");
            Res::new(201)
        }
        ("DELETE", "/v2/a/b/manifests/t") => Res::new(202),
        _ => Res::new(404).body(r#"{"errors":[{"code":"MANIFEST_UNKNOWN","message":"nope"}]}"#),
    });
    let c = client();
    let rp = repo(&f.addr);
    let got = c.get_manifest(&rp.tag("t")).unwrap();
    assert_eq!(
        got,
        Fetched {
            bytes: b"{}".to_vec(),
            content_type: media::OCI_MANIFEST.into(),
            digest: digest::of(b"{}")
        }
    );
    assert_eq!(c.get_manifest(&rp.tag("nct")).unwrap().content_type, "");
    let err = format!("{:#}", c.get_manifest(&rp.digest(D)).unwrap_err());
    assert!(err.contains("does not match"), "{err}");
    let err = format!("{:#}", c.get_manifest(&rp.tag("missing")).unwrap_err());
    assert!(
        err.contains("404") && err.contains("MANIFEST_UNKNOWN: nope"),
        "{err}"
    );

    assert_eq!(
        c.head_manifest(&rp.tag("t")).unwrap(),
        Some(Head {
            digest: D.into(),
            content_type: "ct".into(),
            size: 7
        })
    );
    assert_eq!(
        c.head_manifest(&rp.tag("bare")).unwrap(),
        Some(Head {
            digest: D.into(),
            content_type: String::new(),
            size: 0
        })
    );
    assert_eq!(c.head_manifest(&rp.tag("missing")).unwrap(), None);
    assert!(c.head_manifest(&rp.tag("nodigest")).is_err());
    assert!(c.head_manifest(&rp.tag("boom")).is_err());

    assert_eq!(
        c.put_manifest(&rp, "t", b"{}", "ct").unwrap(),
        digest::of(b"{}")
    );
    assert!(c.put_manifest(&rp, "x", b"{}", "ct").is_err());
    c.delete_manifest(&rp.tag("t")).unwrap();
    assert!(c.delete_manifest(&rp.tag("x")).is_err());
}

#[test]
fn blobs() {
    let f = anon(|r| match (r.method.as_str(), r.path.as_str()) {
        ("HEAD", "/v2/a/b/blobs/sha256:yes") => Res::new(200),
        ("HEAD", "/v2/a/b/blobs/sha256:no") => Res::new(404),
        ("HEAD", _) => Res::new(500),
        ("GET", "/v2/a/b/blobs/sha256:yes") => Res::new(200).body("data"),
        _ => Res::new(404),
    });
    let c = client();
    let rp = repo(&f.addr);
    assert!(c.blob_exists(&rp, "sha256:yes").unwrap());
    assert!(!c.blob_exists(&rp, "sha256:no").unwrap());
    assert!(c.blob_exists(&rp, "sha256:err").is_err());
    let mut s = String::new();
    c.get_blob(&rp, "sha256:yes")
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    assert_eq!(s, "data");
    assert!(c.get_blob(&rp, "sha256:no").is_err());
}

#[test]
fn mounts_and_uploads() {
    let f = anon(|r| match (r.method.as_str(), r.path.as_str()) {
        ("POST", p) if p.starts_with("/v2/a/b/blobs/uploads/?mount=sha256%3Ayes&from=c%2Fd") => {
            Res::new(201)
        }
        ("POST", p) if p.starts_with("/v2/a/b/blobs/uploads/?mount=sha256%3Ano") => {
            Res::new(202).header("Location", "/v2/a/b/blobs/uploads/u1")
        }
        ("POST", p) if p.starts_with("/v2/a/b/blobs/uploads/?mount=sha256%3Aerr") => Res::new(500),
        ("POST", "/v2/a/b/blobs/uploads/") => {
            Res::new(202).header("Location", "/v2/a/b/blobs/uploads/u2?state=s")
        }
        ("POST", "/v2/x/y/blobs/uploads/") => Res::new(202),
        ("POST", "/v2/e/f/blobs/uploads/") => Res::new(500),
        ("PUT", "/v2/a/b/blobs/uploads/u1?digest=sha256%3Ano")
        | ("PUT", "/v2/a/b/blobs/uploads/u2?state=s&digest=sha256%3Ad") => {
            assert_eq!(r.header("content-length"), Some("5"));
            assert_eq!(r.body, b"hello");
            Res::new(201)
        }
        _ => Res::new(400),
    });
    let c = client();
    let rp = repo(&f.addr);
    let from = Repository::parse(&format!("{}/c/d", f.addr)).unwrap();
    assert_eq!(c.mount(&rp, "sha256:yes", &from).unwrap(), Mount::Mounted);
    let Mount::Upload(url) = c.mount(&rp, "sha256:no", &from).unwrap() else {
        panic!()
    };
    assert_eq!(url, format!("http://{}/v2/a/b/blobs/uploads/u1", f.addr));
    assert!(c.mount(&rp, "sha256:err", &from).is_err());

    let body = || Box::new(std::io::Cursor::new(b"hello".to_vec())) as Box<dyn Read + Send>;
    c.upload_blob(&rp, "sha256:no", 5, body(), Some(url))
        .unwrap();
    c.upload_blob(&rp, "sha256:d", 5, body(), None).unwrap();
    // PUT rejected
    assert!(c.upload_blob(&rp, "sha256:bad", 5, body(), None).is_err());
    // POST rejected
    let ef = Repository::parse(&format!("{}/e/f", f.addr)).unwrap();
    assert!(c.upload_blob(&ef, "sha256:d", 5, body(), None).is_err());
    // POST without Location
    let xy = Repository::parse(&format!("{}/x/y", f.addr)).unwrap();
    assert!(
        format!(
            "{:#}",
            c.upload_blob(&xy, "sha256:d", 5, body(), None).unwrap_err()
        )
        .contains("no Location")
    );
}

#[test]
fn pagination() {
    let f = anon(|r| match r.path.as_str() {
        "/v2/a/b/tags/list" => Res::new(200)
            .header("Link", r#"</v2/a/b/tags/list?n=1&last=a>; rel="next""#)
            .body(r#"{"name":"a/b","tags":["a"]}"#),
        "/v2/a/b/tags/list?n=1&last=a" => Res::new(200).body(r#"{"name":"a/b","tags":["b"]}"#),
        "/v2/_catalog" => Res::new(200)
            .header(
                "Link",
                r#"<x>; rel="prev", </v2/_catalog?last=r1>; rel="next""#,
            )
            .body(r#"{"repositories":["r1"]}"#),
        "/v2/_catalog?last=r1" => Res::new(200)
            .header("Link", "<y>; rel=\"prev\"")
            .body(r#"{"repositories":null}"#),
        "/v2/bad/json/tags/list" => Res::new(200).body("nope"),
        "/v2/_catalog?bad" => Res::new(200).body("nope"),
        _ => Res::new(404),
    });
    let c = client();
    assert_eq!(c.tags(&repo(&f.addr)).unwrap(), vec!["a", "b"]);
    assert_eq!(c.catalog(&f.addr).unwrap(), vec!["r1"]);
    assert!(
        c.tags(&Repository::parse(&format!("{}/bad/json", f.addr)).unwrap())
            .is_err()
    );
    assert!(
        c.tags(&Repository::parse(&format!("{}/missing", f.addr)).unwrap())
            .is_err()
    );
    let f2 = anon(|_| Res::new(200).body("nope"));
    assert!(c.catalog(&f2.addr).is_err());
    let f3 = anon(|_| Res::new(500));
    assert!(c.catalog(&f3.addr).is_err());
}

#[test]
fn transport_errors_after_ping() {
    let f = anon(|_| Res::drop_conn());
    let c = client();
    let err = format!("{:#}", c.tags(&repo(&f.addr)).unwrap_err());
    assert!(err.contains("/tags/list"), "{err}");
    // an absolute "path" that is not a valid URI
    assert!(
        c.send(
            &f.addr,
            &[],
            "GET",
            "bad uri with spaces",
            &[],
            SendBody::none()
        )
        .is_err()
    );
}

#[test]
fn insecure_client_builds() {
    // disable_verification path; behaviour against a plain HTTP server is the same
    let f = anon(|_| Res::new(200).body(r#"{"name":"a/b","tags":[]}"#));
    assert!(
        Client::new(true, false, Keychain::default())
            .tags(&repo(&f.addr))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn status_error_bodies() {
    let f = anon(|r| match r.path.as_str() {
        "/v2/a/b/tags/list" => Res::new(418),
        _ => Res::new(400).body("  plain text  "),
    });
    let c = client();
    let err = format!("{:#}", c.tags(&repo(&f.addr)).unwrap_err());
    assert!(
        err.ends_with("unexpected status code 418 I'm a teapot"),
        "{err}"
    );
    let err = format!("{:#}", c.catalog(&f.addr).unwrap_err());
    assert!(err.ends_with(": plain text"), "{err}");
}

#[test]
fn error_bodies_large_and_empty_lists() {
    let big = format!(
        r#"{{"errors":[{{"code":"DENIED","message":"{}"}}]}}"#,
        "m".repeat(5000)
    );
    let f = anon(move |r| match r.path.as_str() {
        "/v2/a/b/tags/list" => Res::new(403).body(big.clone()),
        _ => Res::new(400).body(r#"{"errors":[]}"#),
    });
    let c = client();
    // a 5 kB JSON error body is still read and summarised
    assert!(format!("{:#}", c.tags(&repo(&f.addr)).unwrap_err()).contains("DENIED: mmmm"));
    // an empty error list falls back to the raw body
    assert!(format!("{:#}", c.catalog(&f.addr).unwrap_err()).ends_with(r#": {"errors":[]}"#));
}

#[test]
fn helpers() {
    assert_eq!(resolve("http://h:1/v2/x", "/v2/y"), "http://h:1/v2/y");
    assert_eq!(resolve("http://h:1", "/v2/y"), "http://h:1/v2/y");
    assert_eq!(resolve("http://h/v2/x", "https://o/z"), "https://o/z");
    assert_eq!(resolve("http://h/v2/x", "http://o/z"), "http://o/z");
    assert_eq!(percent_encode("a-Z_0.~ /:@"), "a-Z_0.~%20%2F%3A%40");
}

#[test]
fn every_request_kind_reports_transport_errors() {
    let f = anon(|_| Res::drop_conn());
    let c = client();
    let rp = repo(&f.addr);
    let body = || Box::new(std::io::Cursor::new(b"x".to_vec())) as Box<dyn Read + Send>;
    assert!(c.get_manifest(&rp.tag("t")).is_err());
    assert!(c.head_manifest(&rp.tag("t")).is_err());
    assert!(c.put_manifest(&rp, "t", b"{}", "ct").is_err());
    assert!(c.delete_manifest(&rp.tag("t")).is_err());
    assert!(c.blob_exists(&rp, D).is_err());
    assert!(c.get_blob(&rp, D).is_err());
    assert!(c.mount(&rp, D, &rp).is_err());
    assert!(c.upload_blob(&rp, D, 1, body(), None).is_err());
    assert!(
        c.upload_blob(
            &rp,
            D,
            1,
            body(),
            Some(format!("http://{}/v2/a/b/blobs/uploads/u", f.addr))
        )
        .is_err()
    );
    assert!(c.catalog(&f.addr).is_err());
}
