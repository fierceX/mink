//! The embedded workbench must serve the bytes this build produced.
//!
//! `build.rs` publishes the manifest from a verified staging directory; this
//! test starts the real binary and reads `/` plus every asset the index
//! references over HTTP. A size-only check cannot catch a manifest that names
//! files the binary never embedded.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

struct Server {
    child: Child,
    port: u16,
    _home: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self._home);
    }
}

/// A raw HTTP/1.1 GET. `identity` keeps the comparison byte-exact even if the
/// server enables response compression for real browsers.
fn http_get(port: u16, path: &str) -> Option<(u16, Vec<u8>)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).ok()?;
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")?
        + 4;
    let head = String::from_utf8_lossy(&response[..split]).to_string();
    let status = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse::<u16>()
        .ok()?;
    let body = response[split..].to_vec();
    Some((status, body))
}

fn start_server() -> Server {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let home = std::env::temp_dir().join(format!(
        "mink-embedded-web-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&home).unwrap();
    let port = free_port();
    let child = Command::new(env!("CARGO_BIN_EXE_mink-server"))
        .env_remove("MINK_SERVER_HOST")
        .env_remove("MINK_SERVER_PORT")
        .env_remove("MINK_HOME")
        // Serving from disk would bypass the embedded bytes entirely.
        .env_remove("MINK_SERVER_DEV_WEB")
        .env("HOME", &home)
        .env("MINK_SERVER_PORT", port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("mink-server must start");
    let mut server = Server {
        child,
        port,
        _home: home,
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if http_get(port, "/health").is_some_and(|(status, _)| status == 200) {
            return server;
        }
        if Instant::now() >= deadline {
            let _ = server.child.kill();
            panic!("mink-server did not answer /health on port {port}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Every `./assets/...` URL referenced by the built index.
fn asset_refs(index: &str) -> Vec<String> {
    let mut refs = Vec::new();
    let mut rest = index;
    while let Some(start) = rest.find("./assets/") {
        let candidate = &rest[start + 2..];
        let end = candidate
            .find(|ch: char| ch == '"' || ch == '\'' || ch == ')' || ch.is_whitespace())
            .unwrap_or(candidate.len());
        let reference = candidate[..end].to_string();
        if !refs.contains(&reference) {
            refs.push(reference);
        }
        rest = &rest[start + "./assets/".len()..];
    }
    refs
}

#[test]
fn embedded_workbench_serves_the_built_bytes() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/web/dist");
    let index = std::fs::read(dist.join("index.html")).expect(
        "apps/web/dist is missing; the server's build script runs `npm run build`, so build it first",
    );
    assert!(!index.is_empty(), "the built index must not be empty");
    let index_text = String::from_utf8(index.clone()).expect("index.html is UTF-8");
    let refs = asset_refs(&index_text);
    assert!(
        refs.iter().any(|reference| reference.ends_with(".js")),
        "the index must reference a script: {refs:?}"
    );
    assert!(
        refs.iter().any(|reference| reference.ends_with(".css")),
        "the index must reference a stylesheet: {refs:?}"
    );

    let server = start_server();
    let (status, body) = http_get(server.port, "/").expect("index request");
    assert_eq!(status, 200);
    assert_eq!(body, index, "the served index must be the built index");

    for reference in &refs {
        let expected = std::fs::read(dist.join(reference))
            .unwrap_or_else(|error| panic!("{reference} is missing from dist: {error}"));
        let (status, body) =
            http_get(server.port, &format!("/{reference}")).expect("asset request");
        assert_eq!(status, 200, "{reference} must be embedded");
        assert_eq!(body, expected, "{reference} must be served byte for byte");
    }

    // The SPA fallback serves the same index for client-side routes.
    let (status, body) = http_get(server.port, "/goal/somewhere").expect("spa request");
    assert_eq!(status, 200);
    assert_eq!(body, index, "unknown routes fall back to the built index");
}
