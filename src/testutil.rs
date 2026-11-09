//! Test-only scripted HTTP server: every request goes to a handler closure.
//! Keep-alive, Content-Length and chunked request bodies. Status 0 = drop the connection.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Default)]
pub struct Req {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, Default)]
pub struct Res {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Res {
    pub fn new(status: u16) -> Self {
        Self {
            status,
            ..Default::default()
        }
    }
    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
    pub fn body(mut self, b: impl Into<Vec<u8>>) -> Self {
        self.body = b.into();
        self
    }
    /// Drop the connection without answering.
    pub fn drop_conn() -> Self {
        Self::new(0)
    }
}

pub struct Fake {
    pub addr: String,
    pub log: Arc<Mutex<Vec<Req>>>,
}

impl Fake {
    /// "METHOD path" of every request so far.
    pub fn calls(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .map(|r| format!("{} {}", r.method, r.path))
            .collect()
    }
}

type Handler = Arc<dyn Fn(&Req) -> Res + Send + Sync>;

pub fn serve(handler: impl Fn(&Req) -> Res + Send + Sync + 'static) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let log: Arc<Mutex<Vec<Req>>> = Arc::default();
    let handler: Handler = Arc::new(handler);
    let l2 = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (h, l) = (handler.clone(), l2.clone());
            std::thread::spawn(move || {
                let _ = conn(stream, h, l);
            });
        }
    });
    Fake { addr, log }
}

fn conn(stream: TcpStream, handler: Handler, log: Arc<Mutex<Vec<Req>>>) -> std::io::Result<()> {
    let mut w = stream.try_clone()?;
    let mut r = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let mut parts = line.split_whitespace();
        let (Some(method), Some(path), Some(ver)) = (parts.next(), parts.next(), parts.next())
        else {
            return Ok(()); // garbage (e.g. a TLS hello): close
        };
        if !ver.starts_with("HTTP/1.") {
            return Ok(());
        }
        let mut req = Req {
            method: method.into(),
            path: path.into(),
            ..Default::default()
        };
        loop {
            let mut h = String::new();
            r.read_line(&mut h)?;
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                req.headers
                    .push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        if req
            .header("transfer-encoding")
            .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
        {
            loop {
                let mut size = String::new();
                r.read_line(&mut size)?;
                let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16)
                    .unwrap_or(0);
                let mut chunk = vec![0; n + 2];
                r.read_exact(&mut chunk)?;
                if n == 0 {
                    break;
                }
                req.body.extend_from_slice(&chunk[..n]);
            }
        } else if let Some(n) = req
            .header("content-length")
            .and_then(|v| v.parse::<usize>().ok())
        {
            req.body = vec![0; n];
            r.read_exact(&mut req.body)?;
        }
        log.lock().unwrap().push(req.clone());
        let res = handler(&req);
        if res.status == 0 {
            return Ok(());
        }
        let mut out = format!("HTTP/1.1 {} X\r\n", res.status);
        for (k, v) in &res.headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        if req.method != "HEAD"
            || !res
                .headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        {
            out.push_str(&format!("Content-Length: {}\r\n", res.body.len()));
        }
        out.push_str("\r\n");
        w.write_all(out.as_bytes())?;
        if req.method != "HEAD" {
            w.write_all(&res.body)?;
        }
        w.flush()?;
    }
}

/// A port with nothing listening (bound then released).
pub fn dead_addr() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().to_string()
}
