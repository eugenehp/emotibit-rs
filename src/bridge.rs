//! WebSocket bridge for the browser UI.
//!
//! A web page cannot open UDP or raw TCP sockets, so it cannot talk to an EmotiBit directly.
//! This bridge runs natively next to the device connection and gives the page what it can
//! use: a WebSocket carrying the device's packets, plus (optionally) the page itself over
//! plain HTTP on the same port.
//!
//! **Device → page.** JSON text messages:
//!
//! ```json
//! {"type":"hello","device":"MD-V6-0000356","ip":"10.0.0.149"}   ("ip":"Bluetooth" for a BLE board)
//! {"type":"packets","lines":["183422,41,3,PG,1,100,2412,2418,2409", "..."]}
//! {"type":"ack","cmd":"record_begin","ok":true}
//! {"type":"disconnected"}
//! ```
//!
//! Packet lines are in the device's own format, so the page decodes them with the same Rust
//! code (compiled to wasm) the native tools use. Lines are batched every 50 ms.
//!
//! **Page → device.** `{"cmd":"record_begin"}`, `{"cmd":"record_end"}`,
//! `{"cmd":"user_note","text":"..."}`, `{"cmd":"set_power_mode","mode":"low"}`.
//!
//! **Security.** Any web page open in your browser can try `ws://127.0.0.1:<port>`, and these
//! commands include hibernating the device. The bridge therefore only accepts WebSocket
//! connections whose `Origin` is the bridge's own page (or that send no `Origin`, as
//! non-browser clients do), and binds to localhost unless told otherwise.

use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use log::{debug, warn};
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast::error::RecvError;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::Message;

use crate::emotibit_client::EmotiBitHandle;
use crate::status::PowerMode;

#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub addr: SocketAddr,
    /// Directory with the built web UI (`index.html`, `pkg/…`), served over HTTP.
    pub web_dir: Option<PathBuf>,
    /// Extra page origins allowed to connect, e.g. `https://me.github.io` for a statically
    /// hosted copy of the UI. Exact match, opt-in: every origin listed can control the device.
    pub allow_origins: Vec<String>,
}

/// Commands the page may send.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum ClientCmd {
    RecordBegin,
    RecordEnd,
    UserNote { text: String },
    SetPowerMode { mode: String },
}

fn parse_cmd(text: &str) -> Result<ClientCmd> {
    serde_json::from_str(text).context("unrecognised command")
}

/// Whether a WebSocket handshake with this `Origin` should be accepted.
fn origin_allowed(origin: Option<&str>, addr: SocketAddr, extra: &[String]) -> bool {
    let Some(origin) = origin else { return true };
    if extra.iter().any(|o| o.trim_end_matches('/') == origin) {
        return true;
    }
    let port = addr.port();
    let host = origin.strip_prefix("http://").or_else(|| origin.strip_prefix("https://"));
    let Some(host) = host else { return false };
    let allowed = [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
        addr.to_string(),
    ];
    allowed.iter().any(|a| a == host)
}

/// Map a request path to a file under `root`, refusing anything that could leave it.
fn safe_path(root: &Path, request_path: &str) -> Option<PathBuf> {
    let path = request_path.split(['?', '#']).next()?;
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let rel = Path::new(rel);
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(root.join(rel))
}

fn mime(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// Serve the bridge for one device until the listener fails.
pub async fn serve(config: BridgeConfig, handle: EmotiBitHandle) -> Result<()> {
    let listener = TcpListener::bind(config.addr)
        .await
        .with_context(|| format!("cannot listen on {}", config.addr))?;
    let config = Arc::new(config);
    loop {
        let (stream, peer) = listener.accept().await?;
        let (config, handle) = (config.clone(), handle.clone());
        tokio::spawn(async move {
            if let Err(e) = connection(stream, config, handle).await {
                debug!("bridge connection from {peer} ended: {e}");
            }
        });
    }
}

async fn connection(stream: TcpStream, config: Arc<BridgeConfig>, handle: EmotiBitHandle) -> Result<()> {
    // Decide between a WebSocket upgrade and a plain page request by peeking at the headers.
    let mut buf = [0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.peek(&mut buf)).await??;
    let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
    if head.contains("upgrade: websocket") {
        websocket(stream, config, handle).await
    } else {
        http(stream, &config).await
    }
}

async fn http(mut stream: TcpStream, config: &BridgeConfig) -> Result<()> {
    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let mut parts = request.lines().next().unwrap_or_default().split_whitespace();
    let (method, path) = (parts.next().unwrap_or_default(), parts.next().unwrap_or("/"));

    let route = path.split(['?', '#']).next().unwrap_or("/");
    let (status, ctype, body) = if method != "GET" {
        ("405 Method Not Allowed", "text/plain", b"GET only".to_vec())
    } else if route == "/bridge.json" {
        // Lets a page tell "served by a bridge" from "hosted statically".
        ("200 OK", "application/json", br#"{"bridge":true}"#.to_vec())
    } else if let Some(root) = &config.web_dir {
        match safe_path(root, path) {
            Some(file) => match tokio::fs::read(&file).await {
                Ok(bytes) => ("200 OK", mime(&file), bytes),
                Err(_) => ("404 Not Found", "text/plain", b"not found".to_vec()),
            },
            None => ("400 Bad Request", "text/plain", b"bad path".to_vec()),
        }
    } else if crate::webui::is_embedded() {
        let rel = route.trim_start_matches('/');
        let clean = !rel.split('/').any(|c| c == ".." || c == ".");
        match crate::webui::embedded(rel).filter(|_| clean) {
            Some(bytes) => ("200 OK", mime(Path::new(if rel.is_empty() { "index.html" } else { rel })), bytes.to_vec()),
            None => ("404 Not Found", "text/plain", b"not found".to_vec()),
        }
    } else {
        (
            "404 Not Found",
            "text/plain",
            b"emotibit bridge: WebSocket only (no web UI built in; use `emotibit web --web DIR`)".to_vec(),
        )
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&body).await?;
    Ok(())
}

// The handshake callback's error type is tungstenite's, large by design.
#[allow(clippy::result_large_err)]
async fn websocket(stream: TcpStream, config: Arc<BridgeConfig>, handle: EmotiBitHandle) -> Result<()> {
    let addr = config.addr;
    let extra = config.allow_origins.clone();
    let check = move |req: &Request, resp: Response| -> std::result::Result<Response, ErrorResponse> {
        let origin = req.headers().get("origin").and_then(|v| v.to_str().ok());
        if origin_allowed(origin, addr, &extra) {
            Ok(resp)
        } else {
            warn!("rejected WebSocket from origin {origin:?}");
            let mut r = ErrorResponse::new(Some("origin not allowed".into()));
            *r.status_mut() = StatusCode::FORBIDDEN;
            Err(r)
        }
    };
    let ws = tokio_tungstenite::accept_hdr_async(stream, check).await?;
    let (mut tx, mut rx) = ws.split();

    let device = handle.device();
    let addr = if device.is_ble() { "Bluetooth".to_string() } else { device.ip.to_string() };
    let hello = json!({"type": "hello", "device": device.id, "ip": addr});
    tx.send(Message::text(hello.to_string())).await?;

    let mut raw = handle.subscribe_raw();
    let mut batch: Vec<Arc<str>> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    loop {
        tokio::select! {
            line = raw.recv() => match line {
                Ok(l) => batch.push(l),
                Err(RecvError::Lagged(n)) => debug!("browser client lagged by {n} packets"),
                Err(RecvError::Closed) => break,
            },
            _ = tick.tick() => {
                if !batch.is_empty() {
                    let lines: Vec<&str> = batch.iter().map(|l| &**l).collect();
                    tx.send(Message::text(json!({"type": "packets", "lines": lines}).to_string())).await?;
                    batch.clear();
                }
                if !handle.is_connected() {
                    tx.send(Message::text(json!({"type": "disconnected"}).to_string())).await?;
                    break;
                }
            }
            msg = rx.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    let (name, result) = run_command(&handle, text.as_str()).await;
                    let reply = match result {
                        Ok(()) => json!({"type": "ack", "cmd": name, "ok": true}),
                        Err(e) => json!({"type": "ack", "cmd": name, "ok": false, "error": e.to_string()}),
                    };
                    tx.send(Message::text(reply.to_string())).await?;
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
            },
        }
    }
    Ok(())
}

async fn run_command(handle: &EmotiBitHandle, text: &str) -> (String, Result<()>) {
    let cmd = match parse_cmd(text) {
        Ok(c) => c,
        Err(e) => return ("unknown".into(), Err(e)),
    };
    match cmd {
        ClientCmd::RecordBegin => ("record_begin".into(), handle.record_begin().await),
        ClientCmd::RecordEnd => ("record_end".into(), handle.record_end().await),
        ClientCmd::UserNote { text } => ("user_note".into(), handle.user_note(&text).await),
        ClientCmd::SetPowerMode { mode } => (
            "set_power_mode".into(),
            match PowerMode::from_name(&mode) {
                Some(m) => handle.set_power_mode(m).await,
                None => Err(anyhow::anyhow!("unknown power mode {mode:?}")),
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> SocketAddr {
        "127.0.0.1:8765".parse().unwrap()
    }

    #[test]
    fn origin_policy() {
        assert!(origin_allowed(None, addr(), &[]));
        assert!(origin_allowed(Some("http://127.0.0.1:8765"), addr(), &[]));
        assert!(origin_allowed(Some("http://localhost:8765"), addr(), &[]));
        assert!(!origin_allowed(Some("http://evil.example"), addr(), &[]));
        assert!(!origin_allowed(Some("http://localhost:9999"), addr(), &[]));
        assert!(!origin_allowed(Some("null"), addr(), &[]));
        assert!(!origin_allowed(Some("file://"), addr(), &[]));
    }

    #[test]
    fn extra_origins_are_exact_opt_in() {
        let extra = vec!["https://me.github.io/".to_string()];
        assert!(origin_allowed(Some("https://me.github.io"), addr(), &extra));
        assert!(!origin_allowed(Some("https://me.github.io.evil.example"), addr(), &extra));
        assert!(!origin_allowed(Some("https://other.github.io"), addr(), &extra));
    }

    #[test]
    fn static_paths_cannot_escape_the_root() {
        let root = Path::new("/srv/web");
        assert_eq!(safe_path(root, "/"), Some(PathBuf::from("/srv/web/index.html")));
        assert_eq!(safe_path(root, "/pkg/a.wasm?v=1"), Some(PathBuf::from("/srv/web/pkg/a.wasm")));
        assert_eq!(safe_path(root, "/../etc/passwd"), None);
        assert_eq!(safe_path(root, "/a/../../b"), None);
        assert_eq!(safe_path(root, "//etc/passwd"), Some(PathBuf::from("/srv/web/etc/passwd")));
    }

    #[test]
    fn parses_page_commands() {
        assert_eq!(parse_cmd(r#"{"cmd":"record_begin"}"#).unwrap(), ClientCmd::RecordBegin);
        assert_eq!(
            parse_cmd(r#"{"cmd":"user_note","text":"x"}"#).unwrap(),
            ClientCmd::UserNote { text: "x".into() }
        );
        assert_eq!(
            parse_cmd(r#"{"cmd":"set_power_mode","mode":"low"}"#).unwrap(),
            ClientCmd::SetPowerMode { mode: "low".into() }
        );
        assert!(parse_cmd(r#"{"cmd":"format_disk"}"#).is_err());
        assert!(parse_cmd("nope").is_err());
    }

    #[test]
    fn mime_types() {
        assert_eq!(mime(Path::new("a.wasm")), "application/wasm");
        assert_eq!(mime(Path::new("a.js")), "text/javascript; charset=utf-8");
    }
}
