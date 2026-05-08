//! Amp CLI bridge.
//!
//! Stands up a localhost HTTP server that intercepts every call Amp CLI
//! would make to its backend (`AMP_URL`) so:
//!
//! 1. The **LLM plane** (`/api/provider/<protocol>/...`) gets routed to a
//!    user-configured upstream (deepseek, openrouter, etc.). Anthropic-
//!    protocol calls go through aivo's `AnthropicToOpenAIRouter` for
//!    on-the-fly Anthropic→OpenAI translation when the upstream isn't
//!    natively Anthropic.
//! 2. The **management plane** (`/api/internal?<method>`, `/api/user/*`,
//!    `/api/telemetry/*`, `/api/auth/*`) is **stubbed locally by default**
//!    so no traffic leaks to ampcode.com. Stub shapes are mirrored from
//!    real ampcode.com responses so amp's auth check (`isAuthenticated`)
//!    flips true and amp progresses to the LLM call.
//!
//! Setting `AIVO_AMP_PASSTHROUGH=1` flips management traffic to the real
//! ampcode.com endpoint (using the token from `~/.local/share/amp/
//! secrets.json`) — useful if the user wants their thread history /
//! telemetry on Sourcegraph. Off by default for privacy.
//!
//! When `--debug` is on, each request/response is appended to a JSONL
//! trace at `~/.config/aivo/logs/amp-trace-<ts>-<pid>.jsonl`. Without
//! `--debug` the bridge writes nothing to disk. Unhandled paths always
//! emit a loud `[amp-bridge] UNHANDLED` on stderr regardless of `--debug`,
//! so users discovering an unstubbed RPC can re-run with `--debug` to
//! capture the body.

use anyhow::Result;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

use crate::constants::CONTENT_TYPE_JSON;
use crate::services::amp_threads;
use crate::services::http_utils::{self, router_http_client};
use crate::services::percent_codec;

#[derive(Clone)]
pub struct AmpBridgeConfig {
    pub upstream_base_url: String,
    pub upstream_api_key: String,
    /// JSONL file the bridge appends every observed request to.
    /// `Some` only when `--debug` is on; `None` skips trace I/O entirely
    /// so a normal `aivo amp` run touches no log files.
    pub trace_log_path: Option<PathBuf>,
    /// When set, `/api/internal?<method>` and other management routes are
    /// forwarded to a real Amp endpoint (typically `https://ampcode.com/`)
    /// instead of being stubbed. Lets the user keep Amp's auth, threads, and
    /// telemetry plane working against Sourcegraph while only the LLM plane
    /// (`/api/provider/<X>/...`) gets routed at `upstream_base_url`.
    pub native_amp_url: Option<String>,
    pub native_amp_key: Option<String>,
    /// Port of an upstream-targeting `AnthropicToOpenAIRouter` running on
    /// localhost. When set, `/api/provider/anthropic/...` paths are
    /// forwarded to it (translation: Anthropic /v1/messages → OpenAI
    /// /v1/chat/completions). When None, Anthropic requests go directly
    /// to the upstream — only correct when the upstream natively speaks
    /// Anthropic protocol.
    pub anthropic_translation_port: Option<u16>,
    /// Port of an upstream-targeting `ResponsesToChatRouter` running on
    /// localhost. When set, `/api/provider/openai/v1/responses` (the
    /// OpenAI Responses API endpoint amp uses for interactive chat) gets
    /// forwarded there for Responses → /v1/chat/completions translation.
    /// Most non-OpenAI upstreams (deepseek, openrouter, …) only have
    /// /v1/chat/completions, so this translation is mandatory.
    pub responses_translation_port: Option<u16>,
    /// When set, the bridge rewrites the `model` field in `/api/provider/<X>`
    /// request bodies to this value before forwarding. Amp picks Claude
    /// model names internally based on its agent mode; non-Amp upstreams
    /// (deepseek, openrouter, etc.) won't accept those names. Threaded
    /// from `aivo run amp -m <model>`.
    pub force_model: Option<String>,
    /// Directory the bridge persists `uploadThread` payloads to (and
    /// reads back on `getThread` / `listThreads`). Mirrors what
    /// ampcode.com does server-side so `amp threads continue T-<id>`
    /// works after `aivo amp` exits.
    pub threads_dir: PathBuf,
}

pub struct AmpBridge {
    config: AmpBridgeConfig,
}

#[derive(Clone)]
struct AmpBridgeState {
    config: Arc<AmpBridgeConfig>,
    client: reqwest::Client,
}

impl AmpBridge {
    pub fn new(mut config: AmpBridgeConfig) -> Self {
        // Drop the trailing slash once so per-request URL building doesn't
        // re-trim on every forwarded call.
        let trimmed = config.upstream_base_url.trim_end_matches('/').to_string();
        config.upstream_base_url = trimmed;
        if let Some(url) = config.native_amp_url.as_mut() {
            let trimmed = url.trim_end_matches('/').to_string();
            *url = trimmed;
        }
        Self { config }
    }

    /// Binds to a random local port and runs the bridge in the background.
    /// Caller sets `AMP_URL=http://127.0.0.1:<port>` before spawning amp.
    pub async fn start_background(&self) -> Result<(u16, tokio::task::JoinHandle<Result<()>>)> {
        let (listener, port) = http_utils::bind_local_listener().await?;
        let state = AmpBridgeState {
            config: Arc::new(self.config.clone()),
            client: router_http_client(),
        };
        let handle = tokio::spawn(async move { run_bridge(listener, state).await });
        Ok((port, handle))
    }
}

/// Tries to read the user's Sourcegraph Amp token from the canonical
/// `~/.local/share/amp/secrets.json` file. The file format is
/// `{"apiKey@<url>": "<token>"}`. Returns `(url, token)` of the first entry
/// found, or `None` if the file doesn't exist / can't be parsed.
pub fn detect_native_amp_credentials() -> Option<(String, String)> {
    let home = crate::services::system_env::home_dir()?;
    let path = home.join(".local/share/amp/secrets.json");
    let raw = std::fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let obj = value.as_object()?;
    for (k, v) in obj {
        if let Some(token) = v.as_str().filter(|t| !t.is_empty())
            && let Some(url) = k.strip_prefix("apiKey@")
        {
            return Some((url.to_string(), token.to_string()));
        }
    }
    None
}

/// True if `url` points at an Amp-protocol-compatible endpoint that doesn't
/// need the bridge — Sourcegraph's hosted endpoint or anything on localhost
/// (typical of self-hosted Sourcegraph or CLIProxyAPI deployments).
pub fn is_amp_native_endpoint(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    host == "ampcode.com"
        || host.ends_with(".ampcode.com")
        || host == "sourcegraph.com"
        || host.ends_with(".sourcegraph.com")
        || host == "localhost"
        || host == "127.0.0.1"
        || host == "0.0.0.0"
        || host == "::1"
}

/// Response returned by `dispatch`. The streaming variant lets us deliver
/// SSE chunks to amp as they arrive instead of buffering the whole answer
/// — important for interactive chat where token-by-token rendering is
/// the difference between "feels alive" and "stares at a blank screen
/// for 10 seconds".
enum BridgeResponse {
    Buffered {
        status: u16,
        content_type: String,
        body: String,
    },
    Streaming {
        status: u16,
        content_type: String,
        upstream: reqwest::Response,
        /// Apply the reasoning content_part filter incrementally per SSE
        /// event. Set when forwarding `/api/provider/openai/v1/responses`
        /// — those events sometimes carry `part.type == "reasoning"`,
        /// which amp's parser doesn't recognize.
        filter_reasoning: bool,
    },
}

async fn run_bridge(listener: tokio::net::TcpListener, state: AmpBridgeState) -> Result<()> {
    loop {
        let (mut socket, _peer) = listener.accept().await?;
        let state = state.clone();

        tokio::spawn(async move {
            let request_bytes = match http_utils::read_full_request(&mut socket).await {
                Ok(b) => b,
                Err(err) => {
                    let response = http_utils::http_request_read_error_response(&err);
                    let _ = socket.write_all(response.as_bytes()).await;
                    return;
                }
            };

            let request = String::from_utf8_lossy(&request_bytes);
            let method = request.split_whitespace().next().unwrap_or("").to_string();
            let full_path = http_utils::extract_request_path(&request);
            let body = http_utils::extract_request_body(&request)
                .unwrap_or("")
                .to_string();

            log_request(
                state.config.trace_log_path.as_deref(),
                &method,
                &full_path,
                &body,
            )
            .await;

            let dispatch_result = dispatch(&state, &request, &method, &full_path, &body).await;
            match dispatch_result {
                Ok(BridgeResponse::Buffered {
                    status,
                    content_type,
                    body,
                }) => {
                    log_response_buffered(
                        state.config.trace_log_path.as_deref(),
                        &full_path,
                        status,
                        &body,
                    )
                    .await;
                    let _ = http_utils::write_buffered_response(
                        &mut socket,
                        status,
                        &content_type,
                        body.as_bytes(),
                    )
                    .await;
                }
                Ok(BridgeResponse::Streaming {
                    status,
                    content_type,
                    upstream,
                    filter_reasoning,
                }) => {
                    let captured = stream_through_socket(
                        &mut socket,
                        status,
                        &content_type,
                        upstream,
                        filter_reasoning,
                    )
                    .await;
                    log_response_buffered(
                        state.config.trace_log_path.as_deref(),
                        &full_path,
                        status,
                        &captured,
                    )
                    .await;
                }
                Err(err) => {
                    eprintln!("[amp-bridge] dispatch error: {err}");
                    let raw = http_utils::http_error_response(500, "amp-bridge error");
                    let _ = socket.write_all(raw.as_bytes()).await;
                }
            }
        });
    }
}

/// Streams `upstream` through `socket` as chunked HTTP. Captures the bytes
/// in a buffer (returned to the caller for trace logging) while writing
/// them to the socket — buffer growth is bounded by the upstream's natural
/// response size. When `filter_reasoning` is set, runs the SSE byte stream
/// through `IncrementalReasoningFilter` so events with
/// `part.type == "reasoning"` are dropped before they reach amp.
async fn stream_through_socket(
    socket: &mut tokio::net::TcpStream,
    status: u16,
    content_type: &str,
    mut upstream: reqwest::Response,
    filter_reasoning: bool,
) -> String {
    let head = http_utils::http_chunked_response_head(status, content_type);
    if socket.write_all(head.as_bytes()).await.is_err() {
        return String::new();
    }
    let mut captured = String::new();
    let mut filter = IncrementalReasoningFilter::new();
    while let Ok(Some(chunk)) = upstream.chunk().await {
        let bytes = if filter_reasoning {
            filter.feed(&chunk)
        } else {
            chunk.to_vec()
        };
        if !bytes.is_empty() {
            captured.push_str(&String::from_utf8_lossy(&bytes));
            let formatted = http_utils::format_http_chunk(&bytes);
            if socket.write_all(&formatted).await.is_err() {
                break;
            }
        }
    }
    if filter_reasoning {
        let tail = filter.flush();
        if !tail.is_empty() {
            captured.push_str(&String::from_utf8_lossy(&tail));
            let formatted = http_utils::format_http_chunk(&tail);
            let _ = socket.write_all(&formatted).await;
        }
    }
    let _ = socket.write_all(b"0\r\n\r\n").await;
    captured
}

/// Streaming SSE filter: buffers incoming bytes, emits complete events
/// (delimited by `\n\n`) one at a time after running each through the
/// reasoning strip. Partial events stay in the buffer until the next
/// `feed()` call. `flush()` emits whatever's left at end-of-stream.
struct IncrementalReasoningFilter {
    buffer: String,
}

impl IncrementalReasoningFilter {
    fn new() -> Self {
        Self {
            buffer: String::new(),
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        let mut out = Vec::new();
        while let Some(idx) = self.buffer.find("\n\n") {
            let event = self.buffer[..idx].to_string();
            self.buffer.drain(..idx + 2);
            if event_is_reasoning_content_part(&event) {
                continue;
            }
            let cleaned = strip_reasoning_from_event_data(&event);
            out.extend_from_slice(cleaned.as_bytes());
            out.extend_from_slice(b"\n\n");
        }
        out
    }

    fn flush(&mut self) -> Vec<u8> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        let event = std::mem::take(&mut self.buffer);
        if event_is_reasoning_content_part(&event) {
            return Vec::new();
        }
        let cleaned = strip_reasoning_from_event_data(&event);
        cleaned.into_bytes()
    }
}

async fn log_request(path: Option<&Path>, method: &str, full_path: &str, body: &str) {
    let Some(path) = path else { return };
    let entry = json!({
        "ts": http_utils::current_unix_ts(),
        "phase": "request",
        "method": method,
        "path": full_path,
        "body": body,
    });
    append_trace(path, &entry).await;
}

async fn log_response_buffered(path: Option<&Path>, full_path: &str, status: u16, body: &str) {
    let Some(path) = path else { return };
    let entry = json!({
        "ts": http_utils::current_unix_ts(),
        "phase": "response",
        "path": full_path,
        "status": status,
        "body": body,
    });
    append_trace(path, &entry).await;
}

async fn append_trace(path: &Path, entry: &serde_json::Value) {
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    if let Ok(mut f) = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
    {
        let line = format!("{entry}\n");
        let _ = f.write_all(line.as_bytes()).await;
    }
}

async fn dispatch(
    state: &AmpBridgeState,
    request: &str,
    method: &str,
    full_path: &str,
    body: &str,
) -> Result<BridgeResponse> {
    let (path, query) = match full_path.split_once('?') {
        Some((p, q)) => (p, q),
        None => (full_path, ""),
    };

    if let Some(rest) = path.strip_prefix("/api/provider/") {
        return forward_to_upstream(state, rest, body, request).await;
    }

    // Management surface: if the user has a real Amp account configured,
    // forward to the real endpoint so amp's auth/threads/telemetry plane
    // works for real. Otherwise fall back to stubs.
    let is_management = path == "/api/internal"
        || path.starts_with("/api/user")
        || path.starts_with("/api/telemetry")
        || path.starts_with("/api/otel")
        || path.starts_with("/api/auth");
    if is_management
        && state.config.native_amp_url.is_some()
        && state.config.native_amp_key.is_some()
    {
        return forward_to_native_amp(state, full_path, body, request).await;
    }

    if path == "/api/internal" {
        let body_text = handle_internal_rpc(state, query, body).await;
        return Ok(stub_buffered(body_text, CONTENT_TYPE_JSON));
    }

    if path.starts_with("/api/user") {
        return Ok(stub_buffered(
            r#"{"userEmail":"aivo@local","isInternalUser":false,"features":[],"team":null,"mysteriousMessage":""}"#.to_string(),
            CONTENT_TYPE_JSON,
        ));
    }

    if path.starts_with("/api/telemetry")
        || path.starts_with("/api/otel")
        || path.starts_with("/api/auth")
    {
        return Ok(stub_buffered("{}".to_string(), CONTENT_TYPE_JSON));
    }

    // Amp polls `<AMP_URL>/news.rss` for announcement banners. Return an
    // empty but well-formed feed so the check is a silent no-op.
    if path == "/news.rss" {
        return Ok(stub_buffered(
            r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel><title>aivo</title><link>http://localhost</link><description></description></channel></rss>"#.to_string(),
            "application/rss+xml",
        ));
    }

    eprintln!("[amp-bridge] UNHANDLED: {method} {full_path}");
    if state.config.trace_log_path.is_none() {
        eprintln!("[amp-bridge] re-run with --debug to capture the request body");
    }
    Ok(BridgeResponse::Buffered {
        status: 404,
        content_type: CONTENT_TYPE_JSON.to_string(),
        body: r#"{"error":{"code":"not-found","message":"unhandled by amp-bridge"}}"#.to_string(),
    })
}

fn stub_buffered(body: String, content_type: &str) -> BridgeResponse {
    BridgeResponse::Buffered {
        status: 200,
        content_type: content_type.to_string(),
        body,
    }
}

/// Forwards a management-plane request verbatim to the real Amp endpoint,
/// using the user's stored Sourcegraph token. The path (including query
/// string) is preserved so amp's RPC framework gets a real response.
async fn forward_to_native_amp(
    state: &AmpBridgeState,
    full_path: &str,
    body: &str,
    request: &str,
) -> Result<BridgeResponse> {
    let native_url = state
        .config
        .native_amp_url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("native_amp_url unset"))?;
    let native_key = state
        .config
        .native_amp_key
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("native_amp_key unset"))?;

    let url = format!("{native_url}{full_path}");
    let mut headers = http_utils::extract_passthrough_headers(request)?;
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {native_key}"))?,
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE_JSON));

    let response = state
        .client
        .post(&url)
        .headers(headers)
        .body(body.to_string())
        .send()
        .await?;
    let status = response.status().as_u16();
    let content_type = http_utils::response_content_type(&response);
    if content_type.contains("text/event-stream") {
        return Ok(BridgeResponse::Streaming {
            status,
            content_type,
            upstream: response,
            filter_reasoning: false,
        });
    }
    let resp_body = response.text().await?;
    Ok(BridgeResponse::Buffered {
        status,
        content_type,
        body: resp_body,
    })
}

/// Handles `/api/internal?<method>` requests, persisting and serving
/// thread state on disk for `getThread`/`uploadThread`/`listThreads`/
/// `deleteThread` so amp's resume flow works across `aivo amp`
/// invocations. Everything else delegates to the static stub map.
async fn handle_internal_rpc(state: &AmpBridgeState, query: &str, body: &str) -> String {
    let rpc_method = percent_codec::decode(query);
    let dir = state.config.threads_dir.as_path();
    match rpc_method.as_str() {
        "uploadThread" => {
            // Capture the FULL thread payload amp uploads on every turn
            // so getThread/listThreads can serve real data.
            if let Some(payload) = amp_threads::extract_thread_payload_from_request(body)
                && let Err(err) = amp_threads::save_thread(dir, &payload).await
            {
                eprintln!("[amp-bridge] uploadThread save failed: {err}");
            }
            r#"{"ok":true}"#.to_string()
        }
        "getThread" => {
            let Some(id) = amp_threads::extract_thread_id_from_request(body) else {
                return r#"{"ok":false,"error":{"code":"thread-not-found","message":"Thread not found"}}"#
                    .to_string();
            };
            match amp_threads::load_thread(dir, &id).await {
                Some(payload) => json!({
                    "ok": true,
                    "result": {"thread": {"data": payload}},
                })
                .to_string(),
                None => r#"{"ok":false,"error":{"code":"thread-not-found","message":"Thread not found"}}"#
                    .to_string(),
            }
        }
        "listThreads" => {
            let limit = amp_threads::extract_list_limit(body);
            let threads = amp_threads::list_threads(dir, limit).await;
            json!({"ok": true, "result": {"threads": threads}}).to_string()
        }
        "deleteThread" => {
            if let Some(id) = amp_threads::extract_thread_id_from_request(body) {
                amp_threads::delete_thread(dir, &id).await;
            }
            r#"{"ok":true,"result":null}"#.to_string()
        }
        _ => internal_rpc_stub_body(query),
    }
}

fn internal_rpc_stub_body(query: &str) -> String {
    // Amp's RPC envelope (captured from real ampcode.com responses): caller
    // expects `{ok: true, result: ...}` or `{ok: false, error: {...}}`.
    // Stubs below mirror the real response shapes so amp considers itself
    // authenticated and proceeds, without any network traffic to
    // ampcode.com.
    let rpc_method = percent_codec::decode(query);
    match rpc_method.as_str() {
        "getUserInfo" => {
            // Schema mirrored from a real ampcode.com response. Auth check
            // on the amp client side requires a non-empty `email` and the
            // `accept-abuse-data-retention` feature flag.
            //
            // `isInternalUser: true` unlocks experimental agent modes and
            // honors `amp.internal.*` settings (notably `internal.model`,
            // which lets the user override the primary model — handy for
            // pointing amp at a Gemini-3 catalog entry to bypass the
            // ~300k context cap on Claude Opus while the bridge actually
            // serves requests via the configured upstream).
            r#"{"ok":true,"result":{"id":"user_aivo_local","username":null,"githubLogin":null,"slackUserID":null,"email":"aivo@local","firstName":"aivo","lastName":"local","emailVerified":true,"profilePictureUrl":null,"lastSignInAt":"2026-01-01T00:00:00.000Z","createdAt":"2026-01-01T00:00:00.000Z","updatedAt":"2026-01-01T00:00:00.000Z","siteAdmin":true,"isInternalUser":true,"features":[{"name":"accept-abuse-data-retention","enabled":true}],"mysteriousMessage":null}}"#.to_string()
        }
        "loadPlugins" => r#"{"ok":true,"result":[]}"#.to_string(),
        "getUserFreeTierStatus" => r#"{"ok":true,"result":{"canUseAmpFree":false}}"#.to_string(),
        // amp's resume flow calls `getThreadLinkInfo` for two checks:
        // (1) `result.creatorUserID` matched against the viewer to gate
        //     "cannot resume thread created by another user". We pin
        //     `creatorUserID` to the same `user_aivo_local` id we hand
        //     out in `getUserInfo` so ownership always matches.
        // (2) `result.usesThreadActors` — if true, amp refuses to resume
        //     the thread in the legacy CLI ("created with the Neo TUI").
        //     Aivo doesn't drive Neo, so always false.
        // Without this stub, the generic `result:null` arm below caused
        // amp's resume to throw "Unexpected error inside Amp CLI" while
        // dereferencing `null.creatorUserID`.
        "getThreadLinkInfo" => {
            r#"{"ok":true,"result":{"creatorUserID":"user_aivo_local","usesThreadActors":false}}"#
                .to_string()
        }
        // `getThread` / `uploadThread` / `listThreads` / `deleteThread`
        // are intercepted by `handle_internal_rpc` for disk persistence
        // before reaching this stub. They never fall through here.
        // amp's server-side LLM-reachable tools — normally executed by
        // ampcode.com, not by the LLM. The bridge has no implementation,
        // and the previous generic `result:null` stub caused amp's caller
        // code to dereference `result.results` / `result.fullContent` /
        // `result.tasks` and silently fail (red X in the UI), prompting the
        // model to retry. Returning an explicit error makes amp surface a
        // real tool_result error to the model on the first call, so it
        // falls back (Bash/curl for web; in-context TODO list for tasks)
        // immediately instead of looping.
        //
        // Web tools: `web_search` and `read_web_page`.
        // Task tools (single LLM-facing tool, sub-actions create/list/get/
        // update/delete): `createTask`, `listTasks`, `getTask`,
        // `updateTask`, `deleteTask`.
        "webSearch2" | "extractWebPageContent" => {
            r#"{"ok":false,"error":{"code":"not-supported","message":"web search/fetch tools are not implemented in aivo's amp bridge — use Bash with curl instead"}}"#.to_string()
        }
        "createTask" | "listTasks" | "getTask" | "updateTask" | "deleteTask" => {
            r#"{"ok":false,"error":{"code":"not-supported","message":"amp Task tool is not implemented in aivo's amp bridge — track work in-conversation instead"}}"#.to_string()
        }
        _ => {
            // Generic success for unknown methods. The trace log captures
            // the call shape so we can add a real stub later if needed.
            r#"{"ok":true,"result":null}"#.to_string()
        }
    }
}

async fn forward_to_upstream(
    state: &AmpBridgeState,
    rest: &str,
    body: &str,
    request: &str,
) -> Result<BridgeResponse> {
    // rest is e.g. "anthropic/v1/messages", "openai/v1/chat/completions",
    // or "google/v1beta/models/<model>:generateContent".
    let (provider, after) = match rest.split_once('/') {
        Some(parts) => parts,
        None => ("", rest),
    };

    // Single-pass body rewrite:
    // - force-model: amp picks Claude model names from its internal agent
    //   mode; non-Amp upstreams won't recognize them.
    // - strip forced anthropic tool_choice on /api/provider/anthropic: amp's
    //   title-generation call sends `{"type":"tool",...}` which reasoning
    //   models reject with "does not support this tool_choice".
    // Parsing and re-serializing the body is the heaviest per-request cost
    // on large /v1/messages payloads, so do both transforms in one pass.
    let body_owned = rewrite_request_body(
        body,
        state.config.force_model.as_deref(),
        provider == "anthropic",
    );

    // Anthropic-protocol requests route through the in-process translator
    // when the upstream isn't natively Anthropic.
    if provider == "anthropic"
        && let Some(port) = state.config.anthropic_translation_port
    {
        let url = format!("http://127.0.0.1:{port}/{after}");
        return forward_via_url(state, &url, &body_owned, request, false, false).await;
    }

    // OpenAI Responses API (`/v1/responses`) — amp's interactive chat
    // path. Translate to /v1/chat/completions via the responses router,
    // then filter reasoning content_part events on the way back so amp's
    // parser doesn't choke. Streamed when upstream sends SSE.
    if provider == "openai"
        && after.trim_start_matches('/').starts_with("v1/responses")
        && let Some(port) = state.config.responses_translation_port
    {
        let url = format!("http://127.0.0.1:{port}/{after}");
        return forward_via_url(state, &url, &body_owned, request, false, true).await;
    }

    // Direct passthrough — strip the `<provider>/` prefix so the upstream
    // sees a normal request path.
    let url = format!("{}/{after}", state.config.upstream_base_url);
    forward_via_url(state, &url, &body_owned, request, true, false).await
}

/// Strips reasoning-related Responses-API SSE events that amp's parser
/// doesn't recognize. The upstream may emit `response.content_part.added/
/// done` events whose `part.type == "reasoning"` (deepseek-reasoner,
/// gpt-5.5, deepseek-v4-pro at high effort, …). Amp throws "unexpected
/// content_part.added for output message: reasoning" on those. Also
/// strips reasoning entries from `content` arrays in `output_item.done`
/// / `response.completed` snapshots.
fn filter_reasoning_sse(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for chunk in body.split("\n\n") {
        if chunk.is_empty() {
            continue;
        }
        // Drop content_part events whose `part.type == "reasoning"`. JSON
        // key order isn't guaranteed (the upstream may emit
        // `{"reasoning":"","type":"reasoning"}` vs `{"type":"reasoning",...}`),
        // so a substring match on a fixed key order misses cases — parse
        // the data line as JSON and check the part type directly.
        if event_is_reasoning_content_part(chunk) {
            continue;
        }
        // For events that carry a full message snapshot (output_item.done,
        // response.completed), strip reasoning entries from the content
        // array so amp's final-message parser doesn't reject the snapshot.
        let cleaned = strip_reasoning_from_event_data(chunk);
        out.push_str(&cleaned);
        out.push_str("\n\n");
    }
    out
}

fn event_is_reasoning_content_part(chunk: &str) -> bool {
    let Some(json_text) = extract_sse_data(chunk) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json_text) else {
        return false;
    };
    let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if event_type != "response.content_part.added" && event_type != "response.content_part.done" {
        return false;
    }
    value
        .get("part")
        .and_then(|p| p.get("type"))
        .and_then(|t| t.as_str())
        == Some("reasoning")
}

fn extract_sse_data(chunk: &str) -> Option<&str> {
    if let Some(stripped) = chunk.strip_prefix("data: ") {
        return Some(stripped);
    }
    let idx = chunk.find("\ndata: ")?;
    Some(&chunk[idx + "\ndata: ".len()..])
}

fn strip_reasoning_from_event_data(chunk: &str) -> String {
    // SSE event format is `event: <name>\ndata: <json>`. Find the data:
    // line, parse the JSON, surgically remove reasoning content entries,
    // re-emit. Tolerant: if anything goes sideways we return the chunk
    // unmodified rather than corrupting the stream.
    let Some(data_start) = chunk.find("\ndata: ").or_else(|| {
        if chunk.starts_with("data: ") {
            Some(0)
        } else {
            None
        }
    }) else {
        return chunk.to_string();
    };
    let prefix_len = if chunk.starts_with("data: ") {
        "data: ".len()
    } else {
        data_start + "\ndata: ".len()
    };
    let json_text = &chunk[prefix_len..];
    let mut value: serde_json::Value = match serde_json::from_str(json_text) {
        Ok(v) => v,
        Err(_) => return chunk.to_string(),
    };

    let mut changed = false;
    walk_strip_reasoning(&mut value, &mut changed);

    if !changed {
        return chunk.to_string();
    }
    let new_json = value.to_string();
    let mut out = String::with_capacity(chunk.len());
    out.push_str(&chunk[..prefix_len]);
    out.push_str(&new_json);
    out
}

fn walk_strip_reasoning(value: &mut serde_json::Value, changed: &mut bool) {
    match value {
        serde_json::Value::Array(items) => {
            let original_len = items.len();
            items.retain(|v| v.get("type").and_then(|t| t.as_str()) != Some("reasoning"));
            if items.len() != original_len {
                *changed = true;
            }
            for item in items {
                walk_strip_reasoning(item, changed);
            }
        }
        serde_json::Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                walk_strip_reasoning(v, changed);
            }
        }
        _ => {}
    }
}

/// In-place body edits applied in a single parse/serialize pass:
/// - `force_model`: replaces the top-level `model` field when present.
/// - `strip_anthropic_forced_tool_choice`: removes a forced Anthropic-style
///   `{"type":"tool","name":"..."}` tool_choice. `null` and `"auto"` are
///   untouched. Some upstream reasoning models (notably `deepseek-reasoner`)
///   reject any non-`auto` tool_choice with "does not support this
///   tool_choice"; the model still has the tool in `tools[]` and the system
///   prompt usually instructs the behavior, so dropping is safe enough for
///   amp's title-generation case.
/// - **always** rewrites descriptions for `web_search` and `read_web_page`
///   in `tools[]`. The bridge can't serve these — they'd hit a
///   `not-supported` stub mid-conversation. Replacing the schema text
///   with a curl-pointer (~20 tokens vs the original ~100) lets the model
///   see the tool exists but route directly to Bash without a wasted
///   round-trip. amp's system prompt frames web access as a tool-only
///   capability, so stripping the tools entirely caused the model to
///   apologize and give up (2026-05-08 regression). Native amp launches
///   never hit this function (different code path), so ampcode.com's
///   real implementation is unaffected.
///
/// Returns the body verbatim when no edit applies or the body isn't JSON.
fn rewrite_request_body(
    body: &str,
    force_model: Option<&str>,
    strip_anthropic_forced_tool_choice: bool,
) -> String {
    // Cheap substring guard: parsing and re-serializing is the heaviest
    // per-request cost. Skip the round-trip when no edit could possibly
    // apply. False positives (e.g. literal "web_search" inside an unrelated
    // string) just take the slow path and the rewrite no-ops, which is
    // strictly correct.
    let body_might_have_unsupported_tools =
        body.contains("\"web_search\"") || body.contains("\"read_web_page\"");
    if force_model.is_none()
        && !strip_anthropic_forced_tool_choice
        && !body_might_have_unsupported_tools
    {
        return body.to_string();
    }
    let mut value: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return body.to_string(),
    };
    let Some(obj) = value.as_object_mut() else {
        return body.to_string();
    };

    let mut changed = false;
    if let Some(forced) = force_model
        && obj.contains_key("model")
    {
        obj.insert(
            "model".to_string(),
            serde_json::Value::String(forced.to_string()),
        );
        changed = true;
    }
    if strip_anthropic_forced_tool_choice {
        let is_forced = obj
            .get("tool_choice")
            .and_then(|tc| tc.as_object())
            .and_then(|tc| tc.get("type"))
            .and_then(|t| t.as_str())
            == Some("tool");
        if is_forced {
            obj.remove("tool_choice");
            changed = true;
        }
    }
    if rewrite_unsupported_tool_descriptions(obj) {
        changed = true;
    }
    if !changed {
        return body.to_string();
    }
    value.to_string()
}

/// Replaces the `description` field of any `tools[]` entry whose `name` is
/// a bridge-unsupported web tool with a short pointer to Bash + curl.
///
/// Covers two body shapes:
///   - Anthropic Messages: `tools[].name` + `tools[].description`
///   - OpenAI Chat/Responses: `tools[].function.name` + `tools[].function.description`
///
/// Returns `true` if any tool was rewritten.
fn rewrite_unsupported_tool_descriptions(
    obj: &mut serde_json::Map<String, serde_json::Value>,
) -> bool {
    let Some(tools) = obj.get_mut("tools").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    for entry in tools {
        let Some(item) = entry.as_object_mut() else {
            continue;
        };
        // Anthropic shape: `{name, description, input_schema}`
        if let Some(replacement) = item
            .get("name")
            .and_then(|n| n.as_str())
            .and_then(unsupported_tool_replacement)
        {
            item.insert(
                "description".to_string(),
                serde_json::Value::String(replacement.to_string()),
            );
            changed = true;
            continue;
        }
        // OpenAI shape: `{type:"function", function:{name, description, parameters}}`
        if let Some(func) = item.get_mut("function").and_then(|f| f.as_object_mut())
            && let Some(replacement) = func
                .get("name")
                .and_then(|n| n.as_str())
                .and_then(unsupported_tool_replacement)
        {
            func.insert(
                "description".to_string(),
                serde_json::Value::String(replacement.to_string()),
            );
            changed = true;
        }
    }
    changed
}

/// Returns the replacement description for a tool name we know the bridge
/// can't serve, or `None` for any other tool. Wording is identical for
/// both web tools because the actionable workaround is the same and
/// identical strings cache better in the upstream's prompt cache.
///
/// The recommended fetch command is platform-gated at compile time:
/// - Unix (Linux/macOS): `curl` or `wget`, both standard
/// - Windows: `curl.exe` (ships in System32 since Win10 1803) is preferred
///   because PowerShell's bare `curl` is an alias for `Invoke-WebRequest`
///   with incompatible flags. Fall back to `Invoke-WebRequest`/`iwr` when
///   `curl.exe` is unavailable (older Windows / stripped images).
#[cfg(not(windows))]
const WEB_TOOL_REPLACEMENT: &str = "DISABLED in this environment — calling will return an error. \
     To search the web or fetch a URL's contents, use the Bash tool with `curl` or `wget` instead.";
#[cfg(windows)]
const WEB_TOOL_REPLACEMENT: &str = "DISABLED in this environment — calling will return an error. \
     To search the web or fetch a URL's contents, use the Bash tool with `curl.exe` (or PowerShell's `Invoke-WebRequest` / `iwr` if `curl.exe` is unavailable). Note: bare `curl` in PowerShell is an alias for `Invoke-WebRequest` and rejects standard curl flags — always call `curl.exe` explicitly.";
fn unsupported_tool_replacement(name: &str) -> Option<&'static str> {
    match name {
        "web_search" | "read_web_page" => Some(WEB_TOOL_REPLACEMENT),
        _ => None,
    }
}

/// Forwards a request to `url` and returns either a buffered or streaming
/// response based on the upstream's content-type. SSE responses are
/// streamed chunk-by-chunk back to amp so its TUI sees tokens as they
/// arrive — buffering would make the chat feel frozen until the whole
/// answer landed.
///
/// - `inject_auth=true` rewrites the Authorization header with the upstream
///   API key (direct upstream calls). `false` for in-process translator
///   proxies which inject auth themselves.
/// - `filter_reasoning=true` strips `response.content_part.added/done`
///   events whose `part.type == "reasoning"` from the SSE stream and
///   from `output_item.done` / `response.completed` snapshots.
async fn forward_via_url(
    state: &AmpBridgeState,
    url: &str,
    body: &str,
    request: &str,
    inject_auth: bool,
    filter_reasoning: bool,
) -> Result<BridgeResponse> {
    let mut headers = http_utils::extract_passthrough_headers(request)?;
    if inject_auth {
        let auth_value = format!("Bearer {}", state.config.upstream_api_key);
        headers.insert(AUTHORIZATION, HeaderValue::from_str(&auth_value)?);
    }
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE_JSON));

    let response = state
        .client
        .post(url)
        .headers(headers)
        .body(body.to_string())
        .send()
        .await?;
    let status = response.status().as_u16();
    let content_type = http_utils::response_content_type(&response);

    // SSE → stream; everything else → buffer. The reasoning filter still
    // applies in both modes (incremental in streaming, post-hoc in
    // buffered) so amp's parser doesn't see reasoning content_part events
    // either way.
    if content_type.contains("text/event-stream") {
        Ok(BridgeResponse::Streaming {
            status,
            content_type,
            upstream: response,
            filter_reasoning,
        })
    } else {
        let resp_body = response.text().await?;
        let body = if filter_reasoning {
            filter_reasoning_sse(&resp_body)
        } else {
            resp_body
        };
        Ok(BridgeResponse::Buffered {
            status,
            content_type,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_endpoint_detection() {
        assert!(is_amp_native_endpoint("https://ampcode.com/"));
        assert!(is_amp_native_endpoint("https://amp.ampcode.com"));
        assert!(is_amp_native_endpoint("https://ampcode.com"));
        assert!(is_amp_native_endpoint("https://sourcegraph.com/.api/amp/"));
        assert!(is_amp_native_endpoint("http://localhost:8317/"));
        assert!(is_amp_native_endpoint("http://127.0.0.1:8317/"));
        assert!(!is_amp_native_endpoint("https://api.deepseek.com"));
        assert!(!is_amp_native_endpoint("https://openrouter.ai/api/v1"));
        // Path-based spoofing must not pass — host has to be the actual
        // ampcode.com / sourcegraph.com domain (or a subdomain), not a
        // string occurring in the path.
        assert!(!is_amp_native_endpoint(
            "https://attacker.example/ampcode.com"
        ));
        assert!(!is_amp_native_endpoint(
            "https://attacker.example/sourcegraph.com"
        ));
        assert!(!is_amp_native_endpoint(
            "https://ampcode.com.attacker.example"
        ));
        // Garbage input
        assert!(!is_amp_native_endpoint("not a url"));
        assert!(!is_amp_native_endpoint(""));
    }

    #[test]
    fn internal_rpc_stub_known_method_returns_envelope() {
        let body = internal_rpc_stub_body("getUserInfo");
        assert!(body.contains(r#""ok":true"#));
        // Real ampcode.com schema uses `email` (not `userEmail`); amp's auth
        // check requires a non-empty value here to flip isAuthenticated=true.
        assert!(body.contains(r#""email":"aivo@local""#));
        // Required for amp's "data retention accepted" gate.
        assert!(body.contains("accept-abuse-data-retention"));
    }

    #[test]
    fn internal_rpc_stub_unimplemented_llm_tools_return_explicit_error() {
        // The previous generic `{ok:true, result:null}` stub made amp
        // dereference `result.results` / `result.fullContent` /
        // `result.tasks` and silently fail, prompting the model to retry.
        // An explicit error envelope surfaces a real tool_result error so
        // the model falls back on the first call.
        for method in [
            "webSearch2",
            "extractWebPageContent",
            "createTask",
            "listTasks",
            "getTask",
            "updateTask",
            "deleteTask",
        ] {
            let body = internal_rpc_stub_body(method);
            assert!(body.contains(r#""ok":false"#), "{method}");
            assert!(body.contains(r#""code":"not-supported""#), "{method}");
        }
    }

    #[test]
    fn rewrite_request_body_drops_anthropic_forced_selection() {
        let body = r#"{"model":"x","tool_choice":{"type":"tool","name":"set_title","disable_parallel_tool_use":true},"tools":[]}"#;
        let out = rewrite_request_body(body, None, true);
        assert!(!out.contains("tool_choice"));
        assert!(out.contains(r#""model":"x""#));
        assert!(out.contains(r#""tools":[]"#));
    }

    #[test]
    fn rewrite_request_body_passes_through_auto_or_null_tool_choice() {
        // null tool_choice — used by amp's normal chat call. Leave it.
        let body = r#"{"model":"x","tool_choice":null}"#;
        assert_eq!(rewrite_request_body(body, None, true), body);
        // auto tool_choice — leave it.
        let auto = r#"{"model":"x","tool_choice":"auto"}"#;
        assert_eq!(rewrite_request_body(auto, None, true), auto);
    }

    #[test]
    fn rewrite_request_body_replaces_top_level_model() {
        let body = r#"{"model":"claude-haiku-4-5","messages":[{"role":"user","content":"hi"}]}"#;
        let out = rewrite_request_body(body, Some("deepseek-v4-pro"), false);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["model"], "deepseek-v4-pro");
        assert!(parsed["messages"].is_array());
    }

    #[test]
    fn rewrite_request_body_passes_through_invalid_json() {
        let out = rewrite_request_body("not json", Some("x"), true);
        assert_eq!(out, "not json");
    }

    #[test]
    fn rewrite_request_body_applies_both_edits_in_one_pass() {
        let body = r#"{"model":"claude-haiku-4-5","tool_choice":{"type":"tool","name":"x"}}"#;
        let out = rewrite_request_body(body, Some("deepseek-v4-pro"), true);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["model"], "deepseek-v4-pro");
        assert!(parsed.get("tool_choice").is_none());
    }

    #[test]
    fn rewrite_request_body_short_circuits_when_no_edits_requested() {
        let body = r#"{"model":"x","tool_choice":{"type":"tool","name":"y"}}"#;
        assert_eq!(rewrite_request_body(body, None, false), body);
    }

    #[test]
    fn filter_reasoning_drops_content_part_added_events() {
        // Real upstream JSON emits keys in alphabetical order so the part
        // ends up as `{"reasoning":"","type":"reasoning"}` — make sure the
        // filter handles both orderings via JSON parse, not substring match.
        let body = "event: response.content_part.added\n\
                    data: {\"content_index\":1,\"part\":{\"reasoning\":\"\",\"type\":\"reasoning\"},\"type\":\"response.content_part.added\"}\n\n\
                    event: response.output_text.delta\n\
                    data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n";
        let out = filter_reasoning_sse(body);
        assert!(!out.contains("content_part.added"));
        assert!(out.contains("output_text.delta"));
        assert!(out.contains(r#""delta":"hi""#));
    }

    #[test]
    fn filter_reasoning_strips_reasoning_from_content_array_in_snapshot() {
        // response.completed and output_item.done events carry the full
        // assistant message in a `content` array. Reasoning entries there
        // also need to go away.
        let body = "event: response.completed\n\
                    data: {\"type\":\"response.completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"4\"},{\"type\":\"reasoning\",\"reasoning\":\"think\"}]}]}\n\n";
        let out = filter_reasoning_sse(body);
        assert!(out.contains(r#""text":"4""#));
        assert!(!out.contains(r#""type":"reasoning""#));
    }

    #[test]
    fn filter_reasoning_passes_through_when_no_reasoning() {
        let body = "event: response.output_text.delta\n\
                    data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n";
        let out = filter_reasoning_sse(body);
        assert!(out.contains(r#""delta":"hi""#));
    }

    #[test]
    fn rewrite_request_body_passes_through_when_model_absent() {
        let body = r#"{"messages":[]}"#;
        let out = rewrite_request_body(body, Some("x"), false);
        assert_eq!(out, body);
    }

    #[test]
    fn rewrite_request_body_replaces_anthropic_web_tool_descriptions() {
        // Anthropic Messages shape: tools[i] is `{name, description, input_schema}`.
        // The bridge rewrites web_search / read_web_page descriptions to point
        // at Bash + curl since the bridge can't serve them. Other tools (Bash,
        // create_file) are left verbatim.
        let body = r#"{"model":"claude-haiku-4-5","tools":[
            {"name":"web_search","description":"Search the web for current info.","input_schema":{"type":"object"}},
            {"name":"Bash","description":"Run a shell command.","input_schema":{"type":"object"}},
            {"name":"read_web_page","description":"Fetch a URL and return its contents.","input_schema":{"type":"object"}}
        ]}"#;
        let out = rewrite_request_body(body, None, false);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let tools = parsed["tools"].as_array().unwrap();
        assert!(
            tools[0]["description"]
                .as_str()
                .unwrap()
                .contains("DISABLED"),
            "web_search description should be replaced"
        );
        assert!(
            tools[0]["description"].as_str().unwrap().contains("curl"),
            "should mention curl as the workaround"
        );
        assert_eq!(
            tools[1]["description"], "Run a shell command.",
            "Bash description must not be touched"
        );
        assert!(
            tools[2]["description"]
                .as_str()
                .unwrap()
                .contains("DISABLED"),
            "read_web_page description should be replaced"
        );
    }

    #[test]
    #[cfg(windows)]
    fn rewrite_request_body_windows_description_points_at_curl_exe() {
        // Windows-only: bare `curl` in PowerShell is an alias for
        // Invoke-WebRequest and rejects standard curl flags. The model
        // must be steered toward `curl.exe` (ships in System32 since
        // Win10 1803) or PowerShell's Invoke-WebRequest, never bare
        // `curl` in PowerShell.
        let body = r#"{"tools":[{"name":"web_search","description":"x","input_schema":{}}]}"#;
        let out = rewrite_request_body(body, None, false);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let desc = parsed["tools"][0]["description"].as_str().unwrap();
        assert!(desc.contains("curl.exe"));
        assert!(desc.contains("Invoke-WebRequest"));
    }

    #[test]
    #[cfg(not(windows))]
    fn rewrite_request_body_unix_description_keeps_plain_curl() {
        // Unix-only: the description should NOT mention curl.exe or
        // Invoke-WebRequest — those are dead weight on macOS/Linux where
        // bare `curl` and `wget` are universally available.
        let body = r#"{"tools":[{"name":"web_search","description":"x","input_schema":{}}]}"#;
        let out = rewrite_request_body(body, None, false);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let desc = parsed["tools"][0]["description"].as_str().unwrap();
        assert!(desc.contains("curl"));
        assert!(desc.contains("wget"));
        assert!(!desc.contains("curl.exe"));
        assert!(!desc.contains("Invoke-WebRequest"));
    }

    #[test]
    fn rewrite_request_body_replaces_openai_web_tool_descriptions() {
        // OpenAI Chat/Responses shape: tools[i] is `{type:"function", function:{name, description, parameters}}`.
        let body = r#"{"model":"gpt-5","tools":[
            {"type":"function","function":{"name":"web_search","description":"Search.","parameters":{}}},
            {"type":"function","function":{"name":"create_file","description":"Make a file.","parameters":{}}}
        ]}"#;
        let out = rewrite_request_body(body, None, false);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let tools = parsed["tools"].as_array().unwrap();
        assert!(
            tools[0]["function"]["description"]
                .as_str()
                .unwrap()
                .contains("DISABLED")
        );
        assert_eq!(tools[1]["function"]["description"], "Make a file.");
    }

    #[test]
    fn rewrite_request_body_skips_parse_when_no_unsupported_tools_or_edits() {
        // Fast-path: when none of the trigger conditions apply (no
        // force_model, no tool_choice strip, no web_search/read_web_page
        // substring), the function must short-circuit and return the body
        // byte-for-byte. Verifies the substring guard works.
        let body =
            r#"{"model":"claude-haiku-4-5","tools":[{"name":"Bash","description":"shell"}]}"#;
        let out = rewrite_request_body(body, None, false);
        assert_eq!(out, body);
    }

    #[test]
    fn internal_rpc_stub_unknown_method_wraps_in_ok_envelope() {
        // Amp's RPC client checks `response.ok` — a bare null or unwrapped
        // object crashes with `e.ok is not an object`. The stub must wrap.
        let body = internal_rpc_stub_body("someUnknownThing");
        assert_eq!(body, r#"{"ok":true,"result":null}"#);
    }

    #[test]
    fn incremental_reasoning_filter_drops_event_across_chunks() {
        // SSE event arrives split into two reqwest chunks. Filter buffers
        // until the `\n\n` event boundary, then drops the reasoning event.
        let mut filter = IncrementalReasoningFilter::new();
        let part1 = b"event: response.content_part.added\n\
                      data: {\"part\":{\"reasoning\":\"\",\"type\":\"reason";
        let part2 = b"ing\"},\"type\":\"response.content_part.added\"}\n\n\
                      event: response.output_text.delta\n\
                      data: {\"delta\":\"hi\",\"type\":\"response.output_text.delta\"}\n\n";
        let out1 = filter.feed(part1);
        // First chunk has no complete event yet → emit nothing.
        assert!(out1.is_empty());
        let out2 = filter.feed(part2);
        let s = String::from_utf8(out2).unwrap();
        // Reasoning event dropped, output_text.delta passed through.
        assert!(!s.contains("content_part.added"));
        assert!(s.contains(r#""delta":"hi""#));
    }
}
