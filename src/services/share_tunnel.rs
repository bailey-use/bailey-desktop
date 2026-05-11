//! WebSocket tunnel client for `aivo logs share`. Connects to
//! `s.getaivo.dev/_tunnel` (or whatever `AIVO_SHARE_BASE_URL` points at),
//! registers, prints the public URL, and proxies incoming framed HTTP
//! requests back to the local share server. See
//! `s.getaivo.dev/protocol.md` for the wire format.
//!
//! v2 speaks the `aivo-tunnel/2` binary protocol — see `share_codec.rs`.
//! Compared to v1's JSON-over-text + base64 framing:
//!
//! - Response bodies travel as raw bytes, no ~33% base64 inflation.
//! - Per-chunk envelope shrinks from a ~50-byte JSON object to 6 bytes.
//! - Frame parse cost drops from `serde_json::from_str` to a couple of
//!   `take_n` calls on a `&[u8]`.
//!
//! v2 has no auto-reconnect: a dropped tunnel kills the share — closing the
//! local process *is* the unshare mechanism. Re-running `aivo logs share`
//! allocates a fresh slot.
//!
//! The `--debug-local-only` flag in `aivo logs share` skips this whole module
//! and binds the local server on 127.0.0.1 directly.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use http::HeaderValue;
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::services::share_codec::{ClientFrame, METHOD_HEAD, SUBPROTOCOL, ServerFrame};

const DEFAULT_BASE_URL: &str = "https://s.getaivo.dev";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const OUTBOUND_BUFFER: usize = 64;

/// Connect, register, and run the tunnel until either the server drops it
/// or `shutdown` fires (Ctrl+C). Returns Ok on clean shutdown; Err on connect
/// / register failure or on a server-side reject.
///
/// The `shutdown` notify is shared with the local share server so a single
/// Ctrl+C wakes both: the tunnel loop breaks AND every parked long-poll on
/// the local share returns immediately. Without that wiring, Ctrl+C would
/// wait on in-flight `reqwest` calls into the local share's wait window,
/// adding up to 30 seconds before the WS Close reached the public host.
pub async fn run_tunnel(
    local_base: String,
    open_in_browser: bool,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let api_base =
        std::env::var("AIVO_SHARE_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
    let ws_endpoint = format!("{}/_tunnel", http_to_ws_url(&api_base)?);

    let mut req = ws_endpoint
        .as_str()
        .into_client_request()
        .with_context(|| format!("invalid AIVO_SHARE_BASE_URL: {api_base}"))?;
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(SUBPROTOCOL),
    );

    // Connect + handshake takes a network roundtrip; show a spinner so the
    // user sees feedback between Enter and the success banner. The spinner
    // is killed in every exit branch (early returns and the success path);
    // `print_share_started` does the final line-clear.
    let (spinner, spinner_handle) = crate::style::start_spinner(Some(" preparing share…"));

    let connect = tokio::time::timeout(HANDSHAKE_TIMEOUT, connect_async(req)).await;
    let (mut ws, _resp) = match connect {
        Ok(Ok(ok)) => ok,
        Ok(Err(e)) => {
            crate::style::stop_spinner(&spinner);
            let _ = spinner_handle.await;
            return Err(anyhow!(e)).with_context(|| format!("failed to connect to {ws_endpoint}"));
        }
        Err(_) => {
            crate::style::stop_spinner(&spinner);
            let _ = spinner_handle.await;
            return Err(anyhow!("timed out connecting to {ws_endpoint}"));
        }
    };

    // 1. REGISTER — the v2 register frame is `[0x01][info_len: u16][info]`.
    //    `info` is JSON for forward compatibility (more fields can land
    //    here without a wire-format bump).
    let info = format!(
        r#"{{"client":{{"platform":"{}","aivo_version":"{}"}}}}"#,
        std::env::consts::OS,
        env!("CARGO_PKG_VERSION"),
    );
    let register = ClientFrame::Register {
        info_json: info.as_bytes(),
    }
    .encode();
    if let Err(e) = ws.send(Message::Binary(register.into())).await {
        crate::style::stop_spinner(&spinner);
        let _ = spinner_handle.await;
        return Err(e.into());
    }

    // 2. Await REGISTERED (or REJECT).
    let first = match recv_binary(&mut ws, HANDSHAKE_TIMEOUT).await {
        Ok(b) => b,
        Err(e) => {
            crate::style::stop_spinner(&spinner);
            let _ = spinner_handle.await;
            return Err(e);
        }
    };
    let public_url = match ServerFrame::decode(&first) {
        Ok(ServerFrame::Registered { url, .. }) => url,
        Ok(ServerFrame::Reject { reason, .. }) => {
            crate::style::stop_spinner(&spinner);
            let _ = spinner_handle.await;
            return Err(anyhow!("server rejected tunnel: {reason}"));
        }
        Ok(other) => {
            crate::style::stop_spinner(&spinner);
            let _ = spinner_handle.await;
            return Err(anyhow!("unexpected first frame: {other:?}"));
        }
        Err(e) => {
            crate::style::stop_spinner(&spinner);
            let _ = spinner_handle.await;
            return Err(e).context("decode REGISTERED");
        }
    };
    crate::style::stop_spinner(&spinner);
    let _ = spinner_handle.await;
    crate::commands::share::print_share_started(&public_url);
    if open_in_browser {
        let _ = crate::services::browser_open::open_url(&public_url);
    }

    // 3. Split the socket; spawn a writer task so the read loop and the
    //    request-handling tasks can both submit outbound frames concurrently
    //    without contending on a single sink. The writer carries already-
    //    encoded frames (Vec<u8>) — encode happens in the producer.
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(OUTBOUND_BUFFER);
    let (mut sink, mut stream_rx) = ws.split();

    let writer = tokio::spawn(async move {
        while let Some(bytes) = out_rx.recv().await {
            if sink.send(Message::Binary(bytes.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });

    // One reqwest client reused across all proxied requests; reqwest pools
    // connections to the local server so this is cheap.
    let http = reqwest::Client::builder()
        .no_proxy()
        .build()
        .context("build local http client")?;

    let mut exit_reason: Option<String> = None;

    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                println!();
                break;
            }
            msg = stream_rx.next() => {
                let Some(msg) = msg else { break };
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        exit_reason = Some(format!("read error: {e}"));
                        break;
                    }
                };
                let Message::Binary(bytes) = msg else {
                    // Server speaks v2 binary only. A Text frame here would be
                    // a protocol bug; Close is handled below. Anything else
                    // (Ping/Pong WS-level) is dropped silently.
                    if let Message::Close(_) = msg { break; }
                    continue;
                };
                let frame = match ServerFrame::decode(&bytes) {
                    Ok(f) => f,
                    Err(_) => {
                        // Malformed frames from a healthy server shouldn't
                        // happen in v2; don't kill the tunnel on one garbled
                        // message. Same drop-and-continue behavior v1 had.
                        continue;
                    }
                };
                match frame {
                    ServerFrame::Ping => {
                        let _ = out_tx.send(ClientFrame::Pong.encode()).await;
                    }
                    ServerFrame::Request { id, method, path, .. } => {
                        let local = local_base.clone();
                        let http = http.clone();
                        let tx = out_tx.clone();
                        tokio::spawn(async move {
                            proxy_one(&http, &local, id, method, &path, tx).await;
                        });
                    }
                    ServerFrame::Reject { reason, .. } => {
                        exit_reason = Some(format!("server rejected: {reason}"));
                        break;
                    }
                    ServerFrame::Registered { .. } => {
                        // Spec violation (second REGISTERED) — ignore.
                    }
                }
            }
        }
    }

    // Drain the writer so any queued frames (including the close) are sent.
    drop(out_tx);
    let _ = writer.await;

    if let Some(reason) = exit_reason {
        return Err(anyhow!(reason));
    }
    Ok(())
}

/// Handle one framed REQUEST end to end: HTTP the local share server, frame
/// the RESPONSE_HEAD + RESPONSE_CHUNK back over the tunnel.
async fn proxy_one(
    http: &reqwest::Client,
    local_base: &str,
    id: u32,
    method: u8,
    path: &str,
    out_tx: mpsc::Sender<Vec<u8>>,
) {
    let url = format!("{local_base}{path}");
    let builder = match method {
        METHOD_HEAD => http.head(&url),
        _ => http.get(&url),
    };

    let resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
            send_error(&out_tx, id, 502, &format!("local server: {e}")).await;
            return;
        }
    };

    let status = resp.status().as_u16();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|s| (name.as_str().to_string(), s.to_string()))
        })
        .collect();
    let _ = out_tx
        .send(
            ClientFrame::ResponseHead {
                id,
                status,
                headers: &headers,
            }
            .encode(),
        )
        .await;

    let body = resp.bytes().await.unwrap_or_default();
    let _ = out_tx
        .send(
            ClientFrame::ResponseChunk {
                id,
                last: true,
                body: &body,
            }
            .encode(),
        )
        .await;
}

/// Frame a synthetic error response (used when the local server is
/// unreachable). Mirrors the on-wire shape of a real response so the
/// public viewer's error path is unchanged.
async fn send_error(out_tx: &mpsc::Sender<Vec<u8>>, id: u32, status: u16, msg: &str) {
    let headers = vec![("content-type".to_string(), "application/json".to_string())];
    let _ = out_tx
        .send(
            ClientFrame::ResponseHead {
                id,
                status,
                headers: &headers,
            }
            .encode(),
        )
        .await;
    let body = format!("{{\"error\":\"{}\"}}", msg.replace('"', "\\\""));
    let _ = out_tx
        .send(
            ClientFrame::ResponseChunk {
                id,
                last: true,
                body: body.as_bytes(),
            }
            .encode(),
        )
        .await;
}

async fn recv_binary<S>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    timeout: Duration,
) -> Result<Vec<u8>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let msg = tokio::time::timeout(timeout, ws.next())
        .await
        .map_err(|_| anyhow!("timeout waiting for frame"))?
        .ok_or_else(|| anyhow!("server closed before sending a frame"))?
        .context("ws read error")?;
    match msg {
        Message::Binary(b) => Ok(b.to_vec()),
        other => Err(anyhow!("expected binary frame, got {other:?}")),
    }
}

fn http_to_ws_url(base: &str) -> Result<String> {
    let trimmed = base.trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("https://") {
        Ok(format!("wss://{rest}"))
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        Ok(format!("ws://{rest}"))
    } else {
        Err(anyhow!(
            "AIVO_SHARE_BASE_URL must start with http:// or https://: {base}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::http_to_ws_url;

    #[test]
    fn url_scheme_swap() {
        assert_eq!(
            http_to_ws_url("https://s.getaivo.dev").unwrap(),
            "wss://s.getaivo.dev"
        );
        assert_eq!(
            http_to_ws_url("http://127.0.0.1:8080").unwrap(),
            "ws://127.0.0.1:8080"
        );
        assert_eq!(
            http_to_ws_url("https://example.com/").unwrap(),
            "wss://example.com"
        );
        assert!(http_to_ws_url("ftp://example.com").is_err());
    }
}
