//! Single-shot local HTTP server that captures the OAuth
//! `/oauth2callback` redirect for the Gemini Google OAuth flow.
//!
//! Unlike Codex's `codex_oauth_callback` — which must bind a registered
//! port — Google's Installed-App flow accepts *any* loopback port, so we
//! bind an ephemeral `127.0.0.1:0` and thread the assigned port into the
//! authorize URL before printing it.

use anyhow::{Context, Result, anyhow};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::services::gemini_oauth::CALLBACK_PATH;

/// Owns a bound loopback listener + its assigned port. Split from
/// `wait_for_callback` so callers can embed the port in the authorize URL
/// *before* waiting for the redirect.
pub struct LoopbackBinding {
    listener: TcpListener,
    port: u16,
}

impl LoopbackBinding {
    pub fn port(&self) -> u16 {
        self.port
    }
}

pub async fn bind_loopback() -> Result<LoopbackBinding> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("bind Gemini OAuth callback listener on 127.0.0.1:0")?;
    let port = listener.local_addr().context("resolve bound port")?.port();
    Ok(LoopbackBinding { listener, port })
}

pub struct CallbackOutcome {
    pub code: String,
}

/// Waits for one valid `/oauth2callback` hit on the pre-bound listener.
///
/// On state mismatch or error parameter, returns `Err` and the browser sees
/// a 400. On timeout, returns `Err`. Other paths (`/favicon.ico`, etc.) are
/// 404'd and the loop continues.
pub async fn wait_for_callback(
    binding: LoopbackBinding,
    expected_state: &str,
    timeout: Duration,
) -> Result<CallbackOutcome> {
    tokio::time::timeout(timeout, accept_one(binding.listener, expected_state))
        .await
        .map_err(|_| anyhow!("timed out waiting for OAuth callback"))?
}

async fn accept_one(listener: TcpListener, expected_state: &str) -> Result<CallbackOutcome> {
    loop {
        let (mut stream, _) = listener.accept().await.context("accept OAuth callback")?;
        let request_line = match read_request_line(&mut stream).await {
            Ok(line) => line,
            Err(_) => {
                let _ = stream.shutdown().await;
                continue;
            }
        };

        let path_and_query = parse_request_target(&request_line);

        if !path_and_query.starts_with(CALLBACK_PATH) {
            respond(&mut stream, 404, "text/plain; charset=utf-8", b"not found").await;
            continue;
        }

        let query = path_and_query.split_once('?').map(|(_, q)| q).unwrap_or("");
        let (code, state, error) = extract_callback_params(query);

        if let Some(err) = error {
            respond(
                &mut stream,
                400,
                "text/plain; charset=utf-8",
                format!("OAuth error: {err}").as_bytes(),
            )
            .await;
            return Err(anyhow!("OAuth provider returned error: {err}"));
        }

        if state.as_deref() != Some(expected_state) {
            respond(
                &mut stream,
                400,
                "text/plain; charset=utf-8",
                b"state mismatch",
            )
            .await;
            return Err(anyhow!("OAuth callback state mismatch"));
        }

        let code = code.ok_or_else(|| anyhow!("OAuth callback missing `code`"))?;

        respond(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            SUCCESS_HTML.as_bytes(),
        )
        .await;
        return Ok(CallbackOutcome { code });
    }
}

/// Reads up to the first CRLF (or plain LF fallback). Bounded to 8 KiB.
async fn read_request_line(stream: &mut tokio::net::TcpStream) -> Result<String> {
    let mut buf = [0u8; 8192];
    let mut total = 0usize;
    loop {
        let n = stream.read(&mut buf[total..]).await?;
        if n == 0 {
            break;
        }
        total += n;
        if let Some(end) = find_line_end(&buf[..total]) {
            return Ok(String::from_utf8_lossy(&buf[..end]).into_owned());
        }
        if total == buf.len() {
            break;
        }
    }
    Err(anyhow!("request line missing or too long"))
}

fn find_line_end(bytes: &[u8]) -> Option<usize> {
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            let end = if i > 0 && bytes[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            return Some(end);
        }
    }
    None
}

fn parse_request_target(request_line: &str) -> &str {
    let mut parts = request_line.split_whitespace();
    let _method = parts.next();
    parts.next().unwrap_or("")
}

/// Returns `(code, state, error)` from a url-encoded query string.
pub(crate) fn extract_callback_params(
    query: &str,
) -> (Option<String>, Option<String>, Option<String>) {
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => (pair, ""),
        };
        let decoded = crate::services::percent_codec::decode(v);
        match k {
            "code" => code = Some(decoded),
            "state" => state = Some(decoded),
            "error" => error = Some(decoded),
            "error_description" if error.is_none() => error = Some(decoded),
            _ => {}
        }
    }
    (code, state, error)
}

async fn respond(stream: &mut tokio::net::TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "",
    };
    let head = format!(
        "HTTP/1.1 {status} {status_text}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         X-Frame-Options: DENY\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Cache-Control: no-store\r\n\
         \r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body).await;
    let _ = stream.shutdown().await;
}

const SUCCESS_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>aivo — signed in</title>
  <style>
    body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
           background: #0b0b0e; color: #e5e7eb; display: flex; align-items: center;
           justify-content: center; height: 100vh; margin: 0; }
    .card { text-align: center; padding: 2rem 3rem; border: 1px solid #2a2a31;
            border-radius: 12px; background: #141418; }
    h1 { margin: 0 0 .5rem; font-size: 1.25rem; }
    p { margin: 0; color: #9ca3af; font-size: .95rem; }
  </style>
</head>
<body>
  <div class="card">
    <h1>Signed in to Gemini.</h1>
    <p>You can close this tab and return to your terminal.</p>
  </div>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bind_loopback_assigns_nonzero_port() {
        let b = bind_loopback().await.unwrap();
        assert!(b.port() > 0);
    }

    #[test]
    fn parses_request_target() {
        assert_eq!(
            parse_request_target("GET /oauth2callback?code=abc&state=xyz HTTP/1.1"),
            "/oauth2callback?code=abc&state=xyz"
        );
        assert_eq!(parse_request_target("GET / HTTP/1.1"), "/");
    }

    #[test]
    fn extracts_code_and_state() {
        let (code, state, err) = extract_callback_params("code=abc&state=xyz");
        assert_eq!(code.as_deref(), Some("abc"));
        assert_eq!(state.as_deref(), Some("xyz"));
        assert!(err.is_none());
    }

    #[test]
    fn decodes_percent_encoded_code() {
        let (code, _, _) = extract_callback_params("code=a%2Bb%3Dc&state=s");
        assert_eq!(code.as_deref(), Some("a+b=c"));
    }

    #[test]
    fn propagates_error_param() {
        let (code, _, err) = extract_callback_params("error=access_denied");
        assert!(code.is_none());
        assert_eq!(err.as_deref(), Some("access_denied"));
    }

    #[test]
    fn tolerates_empty_query() {
        let (code, state, err) = extract_callback_params("");
        assert!(code.is_none() && state.is_none() && err.is_none());
    }

    #[test]
    fn find_line_end_crlf_and_lf() {
        assert_eq!(find_line_end(b"GET /x\r\n"), Some(6));
        assert_eq!(find_line_end(b"GET /x\n"), Some(6));
        assert_eq!(find_line_end(b"no newline"), None);
    }
}
