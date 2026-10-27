//! Local web interface. Serves the single-page UI and forwards its JSON requests
//! to the same API the CLI uses. Loopback only, token required for the API, and
//! requests must carry a loopback Host header (blocks DNS rebinding).

use crate::api::{self, Request};
use crate::engine::Engine;
use anyhow::{anyhow, Context, Result};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use tiny_http::{Header, Method, Response, Server, StatusCode};

const INDEX: &[u8] = include_bytes!("../web/index.html");
const APP_JS: &[u8] = include_bytes!("../web/app.js");
const APP_CSS: &[u8] = include_bytes!("../web/app.css");
const DOCKVIEW_JS: &[u8] = include_bytes!("../web/vendor/dockview-core.min.js");
const DOCKVIEW_LICENSE: &[u8] = include_bytes!("../web/vendor/LICENSE-dockview-core.md");

/// Largest request body accepted from the UI.
const MAX_BODY: u64 = 1 << 20;

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'";

/// Reads the access token, creating it (32 random bytes, hex) on first use.
/// The file is readable only by its owner.
pub fn load_or_create_token(path: &Path) -> Result<String> {
    if let Ok(existing) = fs::read_to_string(path) {
        let existing = existing.trim().to_string();
        if existing.len() >= 32 {
            return Ok(existing);
        }
    }
    let mut bytes = [0u8; 32];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading /dev/urandom")?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    fs::write(path, format!("{token}\n"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(token)
}

/// Serves the UI until the listener fails. Each request runs on its own thread.
pub fn serve(listen: &str, token: String, engine: Arc<Mutex<Engine>>) -> Result<()> {
    let server = Server::http(listen).map_err(|e| anyhow!("cannot listen on {listen}: {e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .map(|addr| addr.port())
        .ok_or_else(|| anyhow!("web listener has no IP address"))?;
    let allowed_hosts = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    for request in server.incoming_requests() {
        let engine = Arc::clone(&engine);
        let token = token.clone();
        let allowed = allowed_hosts.clone();
        thread::spawn(move || {
            let _ = handle(request, &token, &allowed, &engine);
        });
    }
    Ok(())
}

fn handle(
    mut request: tiny_http::Request,
    token: &str,
    allowed_hosts: &[String],
    engine: &Mutex<Engine>,
) -> std::io::Result<()> {
    let host = header(&request, "Host").unwrap_or_default();
    if !allowed_hosts.iter().any(|h| h == &host) {
        return respond_text(request, 403, "forbidden host");
    }

    let path = request.url().split('?').next().unwrap_or("/").to_string();
    match (request.method(), path.as_str()) {
        (Method::Get, "/") | (Method::Get, "/index.html") => {
            respond_bytes(request, "text/html; charset=utf-8", INDEX)
        }
        (Method::Get, "/app.js") => {
            respond_bytes(request, "text/javascript; charset=utf-8", APP_JS)
        }
        (Method::Get, "/app.css") => respond_bytes(request, "text/css; charset=utf-8", APP_CSS),
        (Method::Get, "/favicon.ico") => respond_text(request, 204, ""),
        (Method::Get, "/vendor/dockview-core.min.js") => {
            respond_bytes(request, "text/javascript; charset=utf-8", DOCKVIEW_JS)
        }
        (Method::Get, "/vendor/LICENSE-dockview-core.md") => {
            respond_bytes(request, "text/plain; charset=utf-8", DOCKVIEW_LICENSE)
        }
        (Method::Post, "/api") => {
            let presented = header(&request, "Authorization")
                .and_then(|v| v.strip_prefix("Bearer ").map(str::to_string))
                .unwrap_or_default();
            if !constant_time_eq(presented.as_bytes(), token.as_bytes()) {
                return respond_json(
                    request,
                    401,
                    r#"{"ok":false,"error":"unauthorized"}"#.into(),
                );
            }
            let mut body = String::new();
            if request
                .as_reader()
                .take(MAX_BODY)
                .read_to_string(&mut body)
                .is_err()
            {
                return respond_json(
                    request,
                    400,
                    r#"{"ok":false,"error":"unreadable body"}"#.into(),
                );
            }
            let response = match serde_json::from_str::<Request>(&body) {
                Ok(req) => {
                    let mut guard = match engine.lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    api::handle(&mut guard, req)
                }
                Err(err) => api::Response::err(format!("bad request: {err}")),
            };
            let text =
                serde_json::to_string(&response).unwrap_or_else(|_| r#"{"ok":false}"#.into());
            respond_json(request, 200, text)
        }
        _ => respond_text(request, 404, "not found"),
    }
}

fn header(request: &tiny_http::Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn security_headers(
    response: Response<std::io::Cursor<Vec<u8>>>,
) -> Response<std::io::Cursor<Vec<u8>>> {
    response
        .with_header(header_of("Content-Security-Policy", CSP))
        .with_header(header_of("X-Content-Type-Options", "nosniff"))
        .with_header(header_of("Referrer-Policy", "no-referrer"))
        .with_header(header_of("Cache-Control", "no-store"))
}

fn header_of(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static header is valid")
}

fn respond_bytes(
    request: tiny_http::Request,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let response =
        Response::from_data(body.to_vec()).with_header(header_of("Content-Type", content_type));
    request.respond(security_headers(response))
}

fn respond_json(request: tiny_http::Request, status: u16, body: String) -> std::io::Result<()> {
    let response = Response::from_string(body)
        .with_status_code(StatusCode(status))
        .with_header(header_of("Content-Type", "application/json"));
    request.respond(security_headers(response))
}

fn respond_text(request: tiny_http::Request, status: u16, body: &str) -> std::io::Result<()> {
    let response = Response::from_string(body)
        .with_status_code(StatusCode(status))
        .with_header(header_of("Content-Type", "text/plain; charset=utf-8"));
    request.respond(security_headers(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpStream;

    fn engine() -> (tempfile::TempDir, Arc<Mutex<Engine>>) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap()
                .success())
        };
        git(&["init", "-q", "-b", "main"]);
        fs::write(repo.join("README.md"), "x\n").unwrap();
        git(&["add", "-A"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "init",
        ]);
        let mut cfg = aura_core::Config::default();
        cfg.repo.path = repo;
        cfg.state_dir = Some(dir.path().join("state"));
        let engine = Engine::open(cfg).unwrap();
        (dir, Arc::new(Mutex::new(engine)))
    }

    /// Starts the server on an ephemeral port and returns the port.
    fn start(token: &str, engine: Arc<Mutex<Engine>>) -> u16 {
        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let token = token.to_string();
        let allowed = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}")];
        thread::spawn(move || {
            for request in server.incoming_requests() {
                let engine = Arc::clone(&engine);
                let token = token.clone();
                let allowed = allowed.clone();
                thread::spawn(move || {
                    let _ = handle(request, &token, &allowed, &engine);
                });
            }
        });
        port
    }

    /// Sends one raw HTTP/1.1 request and returns (status line, body).
    fn raw(port: u16, request: &str) -> (String, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = v.trim().parse().unwrap();
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).unwrap();
        (
            status.trim().to_string(),
            String::from_utf8_lossy(&body).into_owned(),
        )
    }

    fn api_post(token: Option<&str>, host: &str, body: &str) -> String {
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        format!(
            "POST /api HTTP/1.1\r\nHost: {host}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn serves_the_page_with_security_headers() {
        let (_dir, engine) = engine();
        let port = start("t".repeat(40).as_str(), engine);
        let (status, body) = raw(
            port,
            &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
        );
        assert!(status.contains("200"), "{status}");
        assert!(body.contains("<title>Aura</title>"));
    }

    #[test]
    fn api_requires_the_token() {
        let (_dir, engine) = engine();
        let token = "k".repeat(40);
        let port = start(&token, engine);
        let host = format!("127.0.0.1:{port}");
        let (status, _) = raw(port, &api_post(None, &host, r#"{"cmd":"status"}"#));
        assert!(status.contains("401"), "{status}");
        let (status, body) = raw(port, &api_post(Some(&token), &host, r#"{"cmd":"status"}"#));
        assert!(status.contains("200"), "{status}");
        assert!(body.contains("\"ok\":true"), "{body}");
    }

    #[test]
    fn foreign_host_header_is_refused() {
        // DNS rebinding: a page on evil.example resolving to 127.0.0.1 must not reach the API.
        let (_dir, engine) = engine();
        let token = "k".repeat(40);
        let port = start(&token, engine);
        let (status, _) = raw(
            port,
            &api_post(Some(&token), "evil.example", r#"{"cmd":"status"}"#),
        );
        assert!(status.contains("403"), "{status}");
    }

    #[test]
    fn token_file_is_created_private_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web.token");
        let first = load_or_create_token(&path).unwrap();
        assert_eq!(first.len(), 64);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(load_or_create_token(&path).unwrap(), first);
    }
}
