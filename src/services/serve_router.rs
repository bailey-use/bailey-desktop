//! Serve Router — exposes a local OpenAI-compatible HTTP API.
//!
//! Clients send OpenAI-format requests; this router transforms them to whatever
//! protocol the active upstream provider requires, forwards them, and returns
//! OpenAI-format responses.

use anyhow::Result;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::commands::models::fetch_models;
use crate::constants::CONTENT_TYPE_JSON;
use crate::services::copilot_auth::CopilotTokenManager;
use crate::services::http_utils::{self, router_http_client_with_timeout};
use crate::services::log_store::{LogEvent, LogStore};
use crate::services::provider_protocol::{
    ProviderProtocol, fallback_protocols, is_protocol_mismatch, is_terminal_upstream_error,
};
use crate::services::request_log::RequestLogger;
use crate::services::responses_to_chat_router::{
    ResponsesToChatRouterConfig, convert_chat_response_to_responses_sse,
    convert_responses_to_chat_request,
};
use crate::services::serve_responses::{
    OpenAIToResponsesStreamConverter, convert_chat_response_to_responses_json,
    convert_chat_sse_to_responses_sse,
};
use crate::services::serve_upstream::{
    RouterResponse, StreamingBody, UpstreamRequestContext, send_anthropic_chat, send_gemini_chat,
    send_openai_chat, send_openai_embeddings,
};
use crate::services::session_store::ApiKey;

use std::sync::LazyLock;

static HEALTH_RESPONSE: LazyLock<Vec<u8>> = LazyLock::new(|| {
    json!({"status": "ok", "version": crate::version::VERSION})
        .to_string()
        .into_bytes()
});

pub struct ServeRouterConfig {
    pub upstream_base_url: String,
    pub upstream_api_key: String,
    pub upstream_protocol: ProviderProtocol,
    pub is_copilot: bool,
    pub is_openrouter: bool,
    pub is_starter: bool,
    pub cors: bool,
    pub timeout: u64,
    pub auth_token: Option<String>,
    /// Snapshot of model aliases taken at startup. The router rewrites
    /// `body["model"]` against this map before any protocol-specific handler
    /// runs; clients that POST `{"model": "<alias>", ...}` reach the real
    /// upstream model. Edits to aliases after launch require a restart.
    pub aliases: HashMap<String, String>,
}

pub struct ServeRouter {
    config: ServeRouterConfig,
    key: ApiKey,
    log_store: LogStore,
    logger: Option<RequestLogger>,
    failover_keys: Vec<ApiKey>,
}

struct ServeState {
    config: Arc<ServeRouterConfig>,
    client: reqwest::Client,
    key: ApiKey,
    copilot_tokens: Option<Arc<CopilotTokenManager>>,
    active_protocol: Arc<AtomicU8>,
    log_store: LogStore,
    logger: Option<RequestLogger>,
    failover_keys: Arc<Vec<FailoverEntry>>,
    shutdown: Arc<tokio::sync::Notify>,
}

struct FailoverEntry {
    config: Arc<ServeRouterConfig>,
    key: ApiKey,
    copilot_tokens: Option<Arc<CopilotTokenManager>>,
    active_protocol: AtomicU8,
}

impl ServeRouter {
    pub fn new(config: ServeRouterConfig, key: ApiKey, log_store: LogStore) -> Self {
        Self {
            config,
            key,
            log_store,
            logger: None,
            failover_keys: Vec::new(),
        }
    }

    pub fn with_logger(mut self, logger: Option<RequestLogger>) -> Self {
        self.logger = logger;
        self
    }

    pub fn with_failover_keys(mut self, keys: Vec<ApiKey>) -> Self {
        self.failover_keys = keys;
        self
    }

    /// Binds to the port eagerly (propagates "address already in use" immediately),
    /// then spawns the accept loop in the background and returns the join handle
    /// and a shutdown notifier.
    pub async fn start_background(
        self,
        host: &str,
        port: u16,
    ) -> Result<(
        tokio::task::JoinHandle<Result<()>>,
        Arc<tokio::sync::Notify>,
    )> {
        let listener = tokio::net::TcpListener::bind(format!("{}:{}", host, port)).await?;

        let copilot_tokens = if self.config.is_copilot {
            Some(Arc::new(CopilotTokenManager::new(
                self.config.upstream_api_key.clone(),
            )))
        } else {
            None
        };

        let initial_protocol = self.config.upstream_protocol;
        let timeout = self.config.timeout;

        let failover_entries: Vec<FailoverEntry> = self
            .failover_keys
            .into_iter()
            .map(|fk| {
                let profile = crate::services::provider_profile::provider_profile_for_key(&fk);
                let is_copilot = profile.serve_flags.is_copilot;
                let protocol = profile.default_protocol;
                let ct = if is_copilot {
                    Some(Arc::new(CopilotTokenManager::new(
                        fk.key.as_str().to_string(),
                    )))
                } else {
                    None
                };
                FailoverEntry {
                    config: Arc::new(ServeRouterConfig {
                        upstream_base_url:
                            crate::services::provider_profile::resolve_starter_base_url(
                                &fk.base_url,
                            ),
                        upstream_api_key: fk.key.as_str().to_string(),
                        upstream_protocol: protocol,
                        is_copilot,
                        is_openrouter: profile.serve_flags.is_openrouter,
                        is_starter: profile.serve_flags.is_starter,
                        cors: false,
                        timeout,
                        auth_token: None,
                        // aliases resolved on primary before failover runs
                        aliases: HashMap::new(),
                    }),
                    key: fk,
                    copilot_tokens: ct,
                    active_protocol: AtomicU8::new(protocol.to_u8()),
                }
            })
            .collect();

        let shutdown = Arc::new(tokio::sync::Notify::new());

        let state = Arc::new(ServeState {
            config: Arc::new(self.config),
            client: router_http_client_with_timeout(timeout),
            key: self.key,
            copilot_tokens,
            active_protocol: Arc::new(AtomicU8::new(initial_protocol.to_u8())),
            log_store: self.log_store,
            logger: self.logger,
            failover_keys: Arc::new(failover_entries),
            shutdown: shutdown.clone(),
        });

        Ok((tokio::spawn(run_accept_loop(listener, state)), shutdown))
    }
}

async fn run_accept_loop(listener: tokio::net::TcpListener, state: Arc<ServeState>) -> Result<()> {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(100));
    let cors = state.config.cors;
    let cors_extra = if cors {
        http_utils::cors_header_block()
    } else {
        ""
    };
    // Accept both forms Claude Code sends: `Authorization: Bearer <token>`
    // (when ANTHROPIC_AUTH_TOKEN is set) and `x-api-key: <token>` (when
    // ANTHROPIC_API_KEY is set). The two Arcs share lifetime with the loop;
    // both are None when no auth_token is configured.
    let expected_bearer: Option<Arc<str>> = state
        .config
        .auth_token
        .as_ref()
        .map(|t| Arc::from(format!("Bearer {}", t)));
    let expected_token: Option<Arc<str>> = state
        .config
        .auth_token
        .as_ref()
        .map(|t| Arc::from(t.as_str()));

    loop {
        let accept = tokio::select! {
            result = listener.accept() => result,
            _ = state.shutdown.notified() => {
                // Wait for in-flight requests to finish (max 5s)
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    semaphore.acquire_many(100),
                ).await;
                return Ok(());
            }
        };
        let (mut socket, peer_addr) = accept?;
        let peer_ip = peer_addr.ip().to_string();
        let state = state.clone();
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => continue, // semaphore closed during shutdown
        };
        let expected_bearer = expected_bearer.clone();
        let expected_token = expected_token.clone();

        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;

            let _permit = permit;
            let read_result = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                http_utils::read_full_request(&mut socket),
            )
            .await;

            let request_bytes = match read_result {
                Ok(Ok(b)) => b,
                Ok(Err(err)) => {
                    let response = http_utils::http_request_read_error_response(&err);
                    let _ = socket.write_all(response.as_bytes()).await;
                    return;
                }
                Err(_) => {
                    let _ = socket
                        .write_all(
                            http_utils::http_error_response(408, "Request read timed out")
                                .as_bytes(),
                        )
                        .await;
                    return;
                }
            };

            let request = String::from_utf8_lossy(&request_bytes).into_owned();

            // Handle OPTIONS preflight for CORS
            if cors && request.starts_with("OPTIONS ") {
                let head =
                    http_utils::http_response_head_with_extra(204, "text/plain", 0, cors_extra);
                let _ = socket.write_all(head.as_bytes()).await;
                return;
            }

            let path = http_utils::extract_request_path(&request);
            let path_no_query = path.split('?').next().unwrap_or(&path);

            // Auth check (skip /health). Claude Code sends `Authorization:
            // Bearer <token>` for ANTHROPIC_AUTH_TOKEN and `x-api-key: <token>`
            // for ANTHROPIC_API_KEY; accept either against the configured token.
            if let (Some(bearer), Some(token)) = (&expected_bearer, &expected_token)
                && path_no_query != "/health"
            {
                let headers_end = request.find("\r\n\r\n").unwrap_or(request.len());
                let head = &request[..headers_end];
                let auth_header = http_utils::header_value(head, "Authorization");
                let api_key_header = http_utils::header_value(head, "x-api-key");
                let bearer_match = auth_header == Some(&**bearer);
                let api_key_match = api_key_header == Some(&**token);
                if !bearer_match && !api_key_match {
                    let _ = socket
                        .write_all(
                            http_utils::http_error_response(
                                401,
                                "Invalid or missing auth token (expected Authorization: Bearer or x-api-key)",
                            )
                            .as_bytes(),
                        )
                        .await;
                    return;
                }
            }

            let request_start = std::time::Instant::now();

            // Extract model from request body for logging (best-effort)
            let log_model = http_utils::extract_request_body(&request)
                .ok()
                .and_then(|body| serde_json::from_str::<Value>(body).ok())
                .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from));

            let result = match path_no_query {
                "/health" => Ok(RouterResponse::buffered(
                    200,
                    CONTENT_TYPE_JSON,
                    HEALTH_RESPONSE.clone(),
                )),
                "/v1/models" | "/models" => handle_models(&state).await,
                "/v1/chat/completions" => {
                    if !request.starts_with("POST ") {
                        Ok(RouterResponse::buffered(
                            405,
                            CONTENT_TYPE_JSON,
                            br#"{"error":{"message":"Method not allowed"}}"#.to_vec(),
                        ))
                    } else {
                        handle_chat_with_failover(&request, &state).await
                    }
                }
                "/v1/responses" | "/responses" => {
                    if !request.starts_with("POST ") {
                        Ok(RouterResponse::buffered(
                            405,
                            CONTENT_TYPE_JSON,
                            br#"{"error":{"message":"Method not allowed"}}"#.to_vec(),
                        ))
                    } else {
                        handle_responses_with_failover(&request, &state).await
                    }
                }
                "/v1/embeddings" => {
                    if !request.starts_with("POST ") {
                        Ok(RouterResponse::buffered(
                            405,
                            CONTENT_TYPE_JSON,
                            br#"{"error":{"message":"Method not allowed"}}"#.to_vec(),
                        ))
                    } else {
                        handle_embeddings_with_failover(&request, &state).await
                    }
                }
                _ => Ok(RouterResponse::buffered(
                    404,
                    CONTENT_TYPE_JSON,
                    br#"{"error":{"message":"Not found"}}"#.to_vec(),
                )),
            };

            let response_status = match &result {
                Ok(RouterResponse::Buffered { status, .. }) => *status,
                Ok(RouterResponse::Streaming { status, .. }) => *status,
                Err(_) => 500,
            };

            match result {
                Ok(response) => {
                    let _ = write_router_response(&mut socket, response, cors_extra).await;
                }
                Err(e) => {
                    let _ = socket
                        .write_all(http_utils::http_error_response(500, &e.to_string()).as_bytes())
                        .await;
                }
            }

            // Log request (non-blocking, non-fatal)
            let latency_ms = request_start.elapsed().as_millis();
            let method = request
                .split_whitespace()
                .next()
                .unwrap_or("GET")
                .to_string();

            if let Some(ref logger) = state.logger {
                logger
                    .log(crate::services::request_log::RequestLogEntry {
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        method: method.clone(),
                        path: path_no_query.to_string(),
                        model: log_model.clone(),
                        status: response_status,
                        latency_ms: latency_ms as u64,
                        ip: peer_ip.clone(),
                    })
                    .await;
            }

            let _ = state
                .log_store
                .append(LogEvent {
                    source: "serve".to_string(),
                    kind: "serve_request".to_string(),
                    key_id: Some(state.key.id.clone()),
                    key_name: Some(state.key.display_name().to_string()),
                    base_url: Some(state.key.base_url.clone()),
                    tool: Some("serve".to_string()),
                    model: log_model,
                    status_code: Some(response_status as i64),
                    duration_ms: Some(latency_ms as i64),
                    title: Some(format!("{method} {path_no_query}")),
                    payload_json: Some(json!({
                        "method": method,
                        "path": path_no_query,
                        "ip": peer_ip,
                    })),
                    ..Default::default()
                })
                .await;
        });
    }
}

async fn handle_models(state: &ServeState) -> Result<RouterResponse> {
    let models = fetch_models(&state.client, &state.key).await?;
    let mut data: Vec<Value> = models
        .into_iter()
        .map(|id| json!({"id": id, "object": "model", "owned_by": "aivo"}))
        .collect();
    let mut alias_names: Vec<&String> = state.config.aliases.keys().collect();
    alias_names.sort();
    for name in alias_names {
        data.push(json!({"id": name, "object": "model", "owned_by": "aivo-alias"}));
    }
    let resp = json!({"object": "list", "data": data});
    Ok(RouterResponse::buffered(
        200,
        CONTENT_TYPE_JSON,
        resp.to_string().into_bytes(),
    ))
}

fn parse_json_body(body_str: &str) -> std::result::Result<Value, RouterResponse> {
    serde_json::from_str(body_str).map_err(|e| {
        RouterResponse::buffered(
            400,
            CONTENT_TYPE_JSON,
            json!({"error":{"message": format!("Invalid JSON: {}", e)}})
                .to_string()
                .into_bytes(),
        )
    })
}

/// Rewrites `body["model"]` against the alias snapshot, in place. No-op when
/// the field is missing, non-string, empty, or not an alias. Cycles are
/// detected by `resolve_alias_in_memory` and leave the original value.
fn apply_alias(body: &mut Value, aliases: &HashMap<String, String>) {
    if aliases.is_empty() {
        return;
    }
    let Some(model) = body.get("model").and_then(|v| v.as_str()) else {
        return;
    };
    if let Some(resolved) =
        crate::cli_args::resolve_alias_in_memory(aliases, Some(model.to_string()))
        && resolved != model
    {
        body["model"] = Value::String(resolved);
    }
}

async fn handle_chat(request: &str, state: &ServeState) -> Result<RouterResponse> {
    let body_str = http_utils::extract_request_body(request)?;
    let mut body = match parse_json_body(body_str) {
        Ok(v) => v,
        Err(r) => return Ok(r),
    };

    if !body.get("messages").is_some_and(|v| v.is_array()) {
        return Ok(RouterResponse::buffered(
            400,
            CONTENT_TYPE_JSON,
            br#"{"error":{"message":"Missing required field: messages"}}"#.to_vec(),
        ));
    }

    apply_alias(&mut body, &state.config.aliases);
    handle_chat_body(body, state).await
}

async fn handle_responses(request: &str, state: &ServeState) -> Result<RouterResponse> {
    let body_str = http_utils::extract_request_body(request)?;
    let mut body = match parse_json_body(body_str) {
        Ok(v) => v,
        Err(r) => return Ok(r),
    };

    if body.get("input").is_none() {
        return Ok(RouterResponse::buffered(
            400,
            CONTENT_TYPE_JSON,
            br#"{"error":{"message":"Missing required field: input"}}"#.to_vec(),
        ));
    }
    apply_alias(&mut body, &state.config.aliases);
    let original_model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("gpt-4o")
        .to_string();
    let client_wants_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Use `actual_model` to pin the model name to the raw user-supplied value.  The config's
    // `target_protocol` is snapshotted here, before `handle_chat_body` runs the fallback loop;
    // if the loop switches protocol, any protocol-based model-name transformation done by
    // `convert_responses_to_chat_request` would have used the wrong protocol.  Setting
    // `actual_model` causes `select_model_for_protocol` to return it verbatim, so the model
    // field in `chat_body` is always the original string and `handle_chat_body` transforms it
    // for the protocol that is actually selected.
    let mut config = responses_router_config(state);
    config.actual_model = Some(original_model.clone());
    let mut chat_body = convert_responses_to_chat_request(&body, &config);
    chat_body["stream"] = json!(client_wants_stream);
    let chat_response = handle_chat_body(chat_body, state).await?;
    convert_chat_response_for_responses_route(chat_response, client_wants_stream, &original_model)
}

/// Returns true if the status code should trigger failover.
/// - 401/403: auth failure (key revoked, expired, or lacks model access)
/// - 429: rate limited
/// - 5xx: server errors
fn is_failover_status(status: u16) -> bool {
    matches!(status, 401 | 403 | 429) || (500..600).contains(&status)
}

/// Builds a temporary ServeState from a FailoverEntry, sharing the client.
/// Logger is intentionally omitted — failover attempts are not individually logged.
fn failover_state(
    entry: &FailoverEntry,
    client: &reqwest::Client,
    log_store: &LogStore,
) -> ServeState {
    ServeState {
        config: entry.config.clone(), // Arc clone — O(1) atomic increment
        client: client.clone(),
        key: entry.key.clone(),
        copilot_tokens: entry.copilot_tokens.clone(),
        active_protocol: Arc::new(AtomicU8::new(entry.active_protocol.load(Ordering::Relaxed))),
        log_store: log_store.clone(),
        logger: None,
        failover_keys: Arc::new(Vec::new()),
        shutdown: Arc::new(tokio::sync::Notify::new()),
    }
}

/// Generates a failover wrapper around a handler function.
/// Tries the primary handler, then falls through to failover keys on 429/5xx
/// buffered responses. Streaming responses are never retried.
macro_rules! impl_with_failover {
    ($name:ident, $handler:ident) => {
        async fn $name(request: &str, state: &ServeState) -> Result<RouterResponse> {
            let response = $handler(request, state).await?;
            if state.failover_keys.is_empty() {
                return Ok(response);
            }

            let status = match &response {
                RouterResponse::Buffered { status, .. } => *status,
                RouterResponse::Streaming { .. } => return Ok(response),
            };

            if !is_failover_status(status) {
                return Ok(response);
            }

            eprintln!(
                "  \u{21bb} Primary key returned {}; trying failover keys...",
                status
            );
            for entry in state.failover_keys.iter() {
                let fstate = failover_state(entry, &state.client, &state.log_store);
                if let Ok(resp) = $handler(request, &fstate).await {
                    let s = match &resp {
                        RouterResponse::Buffered { status, .. } => *status,
                        RouterResponse::Streaming { .. } => 200,
                    };
                    if !is_failover_status(s) {
                        eprintln!(
                            "  \u{2713} Failover to {} succeeded",
                            entry.key.display_name()
                        );
                        return Ok(resp);
                    }
                }
            }
            Ok(response)
        }
    };
}

impl_with_failover!(handle_chat_with_failover, handle_chat);
impl_with_failover!(handle_responses_with_failover, handle_responses);
impl_with_failover!(handle_embeddings_with_failover, handle_embeddings);

async fn handle_embeddings(request: &str, state: &ServeState) -> Result<RouterResponse> {
    let protocol = ProviderProtocol::from_u8(state.active_protocol.load(Ordering::Relaxed));
    if !matches!(
        protocol,
        ProviderProtocol::Openai | ProviderProtocol::ResponsesApi
    ) {
        return Ok(RouterResponse::buffered(
            501,
            CONTENT_TYPE_JSON,
            br#"{"error":{"message":"Embeddings not supported with this provider"}}"#.to_vec(),
        ));
    }

    let body_str = http_utils::extract_request_body(request)?;
    let mut body = match parse_json_body(body_str) {
        Ok(v) => v,
        Err(r) => return Ok(r),
    };

    apply_alias(&mut body, &state.config.aliases);
    send_openai_embeddings(&body, &upstream_context(state)).await
}

async fn handle_chat_body(body: Value, state: &ServeState) -> Result<RouterResponse> {
    let client_wants_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Skip fallback for copilot/openrouter — these have fixed protocols
    if state.config.is_copilot || state.config.is_openrouter {
        let mut body = body;
        return match ProviderProtocol::from_u8(state.active_protocol.load(Ordering::Relaxed)) {
            ProviderProtocol::Anthropic => {
                handle_chat_anthropic(&body, client_wants_stream, state).await
            }
            ProviderProtocol::Google => {
                handle_chat_gemini(&mut body, client_wants_stream, state).await
            }
            ProviderProtocol::Openai | ProviderProtocol::ResponsesApi => {
                handle_chat_openai(&mut body, client_wants_stream, state).await
            }
        };
    }

    let current = ProviderProtocol::from_u8(state.active_protocol.load(Ordering::Relaxed));
    let candidates: Vec<ProviderProtocol> = std::iter::once(current)
        .chain(fallback_protocols(current))
        .collect();

    let mut first_error: Option<RouterResponse> = None;
    for (attempt, protocol) in candidates.into_iter().enumerate() {
        let mut body_clone = body.clone();
        let response = match protocol {
            ProviderProtocol::Anthropic => {
                handle_chat_anthropic(&body_clone, client_wants_stream, state).await?
            }
            ProviderProtocol::Google => {
                handle_chat_gemini(&mut body_clone, client_wants_stream, state).await?
            }
            ProviderProtocol::Openai | ProviderProtocol::ResponsesApi => {
                handle_chat_openai(&mut body_clone, client_wants_stream, state).await?
            }
        };

        let status = match &response {
            RouterResponse::Buffered { status, .. } => *status,
            // Streaming is only produced when the upstream returned 200 (see each handle_chat_* handler);
            // a protocol mismatch (404/405/415) always results in a Buffered error response.
            RouterResponse::Streaming { .. } => 200,
        };

        if is_protocol_mismatch(status) {
            let is_terminal = is_terminal_upstream_error(status);
            if is_terminal || first_error.is_none() {
                first_error = Some(response);
            }
            if is_terminal {
                // Pin in-memory so concurrent/retry requests skip the
                // wrong-path probes and hit this protocol directly.
                state
                    .active_protocol
                    .store(protocol.to_u8(), Ordering::Relaxed);
                break;
            }
            continue;
        }

        // Not a mismatch — return this response
        if attempt > 0 {
            state
                .active_protocol
                .store(protocol.to_u8(), Ordering::Relaxed);
            eprintln!("  \u{2022} Protocol auto-switched to {}", protocol.as_str());
        }
        return Ok(response);
    }

    Ok(first_error.unwrap_or(RouterResponse::buffered(
        503,
        CONTENT_TYPE_JSON,
        br#"{"error":{"message":"No compatible protocol found"}}"#.to_vec(),
    )))
}

fn responses_router_config(state: &ServeState) -> ResponsesToChatRouterConfig {
    ResponsesToChatRouterConfig {
        target_base_url: state.config.upstream_base_url.clone(),
        api_key: state.config.upstream_api_key.clone(),
        target_protocol: ProviderProtocol::from_u8(state.active_protocol.load(Ordering::Relaxed)),
        target_path_variant: None,
        copilot_token_manager: state.copilot_tokens.clone(),
        model_prefix: None,
        requires_reasoning_content: false,
        actual_model: None,
        max_tokens_cap: None,
        responses_api_supported: None,
        is_starter: state.config.is_starter,
        aivo_prefix_models: Vec::new(),
    }
}

fn upstream_context(state: &ServeState) -> UpstreamRequestContext {
    UpstreamRequestContext {
        client: state.client.clone(),
        upstream_base_url: state.config.upstream_base_url.clone(),
        upstream_api_key: state.config.upstream_api_key.clone(),
        is_copilot: state.config.is_copilot,
        is_openrouter: state.config.is_openrouter,
        is_starter: state.config.is_starter,
        copilot_tokens: state.copilot_tokens.clone(),
    }
}

async fn handle_chat_anthropic(
    body: &Value,
    client_wants_stream: bool,
    state: &ServeState,
) -> Result<RouterResponse> {
    send_anthropic_chat(body, client_wants_stream, &upstream_context(state)).await
}

async fn handle_chat_gemini(
    body: &mut Value,
    client_wants_stream: bool,
    state: &ServeState,
) -> Result<RouterResponse> {
    send_gemini_chat(body, client_wants_stream, &upstream_context(state)).await
}

async fn handle_chat_openai(
    body: &mut Value,
    client_wants_stream: bool,
    state: &ServeState,
) -> Result<RouterResponse> {
    send_openai_chat(body, client_wants_stream, &upstream_context(state)).await
}

async fn write_router_response(
    socket: &mut tokio::net::TcpStream,
    response: RouterResponse,
    extra_headers: &str,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    match response {
        RouterResponse::Buffered {
            status,
            content_type,
            body,
        } => {
            let headers = http_utils::http_response_head_with_extra(
                status,
                &content_type,
                body.len(),
                extra_headers,
            );
            socket.write_all(headers.as_bytes()).await?;
            socket.write_all(&body).await?;
        }
        RouterResponse::Streaming {
            status,
            content_type,
            body,
        } => {
            let headers = http_utils::http_chunked_response_head_with_extra(
                status,
                &content_type,
                extra_headers,
            );
            socket.write_all(headers.as_bytes()).await?;

            match *body {
                StreamingBody::Upstream(mut upstream) => {
                    while let Some(chunk) = upstream.chunk().await? {
                        write_chunk(socket, &chunk).await?;
                    }
                }
                StreamingBody::Anthropic {
                    mut upstream,
                    mut converter,
                } => {
                    while let Some(chunk) = upstream.chunk().await? {
                        let mapped = converter.push_bytes(&chunk)?;
                        if !mapped.is_empty() {
                            write_chunk(socket, mapped.as_bytes()).await?;
                        }
                    }
                    let tail = converter.finish()?;
                    if !tail.is_empty() {
                        write_chunk(socket, tail.as_bytes()).await?;
                    }
                }
                StreamingBody::Gemini {
                    mut upstream,
                    mut converter,
                } => {
                    while let Some(chunk) = upstream.chunk().await? {
                        let mapped = converter.push_bytes(&chunk)?;
                        if !mapped.is_empty() {
                            write_chunk(socket, mapped.as_bytes()).await?;
                        }
                    }
                    let tail = converter.finish()?;
                    if !tail.is_empty() {
                        write_chunk(socket, tail.as_bytes()).await?;
                    }
                }
                StreamingBody::Responses {
                    source,
                    mut converter,
                } => {
                    match *source {
                        StreamingBody::Upstream(mut upstream) => {
                            while let Some(chunk) = upstream.chunk().await? {
                                let mapped = converter.push_bytes(&chunk)?;
                                if !mapped.is_empty() {
                                    write_chunk(socket, mapped.as_bytes()).await?;
                                }
                            }
                        }
                        StreamingBody::Anthropic {
                            mut upstream,
                            converter: mut openai_converter,
                        } => {
                            while let Some(chunk) = upstream.chunk().await? {
                                let openai = openai_converter.push_bytes(&chunk)?;
                                if !openai.is_empty() {
                                    let mapped = converter.push_bytes(openai.as_bytes())?;
                                    if !mapped.is_empty() {
                                        write_chunk(socket, mapped.as_bytes()).await?;
                                    }
                                }
                            }
                            let openai_tail = openai_converter.finish()?;
                            if !openai_tail.is_empty() {
                                let mapped = converter.push_bytes(openai_tail.as_bytes())?;
                                if !mapped.is_empty() {
                                    write_chunk(socket, mapped.as_bytes()).await?;
                                }
                            }
                        }
                        StreamingBody::Gemini {
                            mut upstream,
                            converter: mut openai_converter,
                        } => {
                            while let Some(chunk) = upstream.chunk().await? {
                                let openai = openai_converter.push_bytes(&chunk)?;
                                if !openai.is_empty() {
                                    let mapped = converter.push_bytes(openai.as_bytes())?;
                                    if !mapped.is_empty() {
                                        write_chunk(socket, mapped.as_bytes()).await?;
                                    }
                                }
                            }
                            let openai_tail = openai_converter.finish()?;
                            if !openai_tail.is_empty() {
                                let mapped = converter.push_bytes(openai_tail.as_bytes())?;
                                if !mapped.is_empty() {
                                    write_chunk(socket, mapped.as_bytes()).await?;
                                }
                            }
                        }
                        StreamingBody::Responses { .. } => {
                            anyhow::bail!("nested responses stream sources are not supported");
                        }
                    }

                    let tail = converter.finish()?;
                    if !tail.is_empty() {
                        write_chunk(socket, tail.as_bytes()).await?;
                    }
                }
            }

            socket.write_all(b"0\r\n\r\n").await?;
        }
    }

    Ok(())
}

async fn write_chunk(socket: &mut tokio::net::TcpStream, chunk: &[u8]) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let formatted = http_utils::format_http_chunk(chunk);
    if formatted.is_empty() {
        return Ok(());
    }
    socket.write_all(&formatted).await?;
    Ok(())
}

fn convert_chat_response_for_responses_route(
    chat_response: RouterResponse,
    client_wants_stream: bool,
    original_model: &str,
) -> Result<RouterResponse> {
    match chat_response {
        RouterResponse::Buffered {
            status,
            content_type,
            body,
        } => {
            if status >= 400 {
                return Ok(RouterResponse::buffered(status, &content_type, body));
            }

            if client_wants_stream {
                let sse = if content_type.contains("text/event-stream") {
                    convert_chat_sse_to_responses_sse(std::str::from_utf8(&body)?, original_model)?
                } else {
                    let chat_json: Value = serde_json::from_slice(&body)?;
                    convert_chat_response_to_responses_sse(&chat_json, false, original_model)
                };
                Ok(RouterResponse::buffered(
                    200,
                    "text/event-stream",
                    sse.into_bytes(),
                ))
            } else {
                let chat_json: Value = serde_json::from_slice(&body)?;
                let response_json =
                    convert_chat_response_to_responses_json(&chat_json, original_model)?;
                Ok(RouterResponse::buffered(
                    200,
                    CONTENT_TYPE_JSON,
                    serde_json::to_vec(&response_json)?,
                ))
            }
        }
        RouterResponse::Streaming {
            status,
            content_type: _,
            body,
        } => {
            if !client_wants_stream {
                anyhow::bail!(
                    "internal error: responses route received streaming body for non-streaming request"
                );
            }

            Ok(RouterResponse::Streaming {
                status,
                content_type: "text/event-stream".to_string(),
                body: Box::new(StreamingBody::Responses {
                    source: body,
                    converter: OpenAIToResponsesStreamConverter::new(original_model),
                }),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::session_store::ApiKey;
    use http::Response as HttpResponse;
    use serde_json::json;

    fn test_key() -> ApiKey {
        ApiKey::new_with_protocol(
            "abc".to_string(),
            "test".to_string(),
            "https://example.com/v1".to_string(),
            None,
            "secret".to_string(),
        )
    }

    fn test_state(protocol: ProviderProtocol) -> ServeState {
        ServeState {
            config: Arc::new(ServeRouterConfig {
                upstream_base_url: "https://example.com/v1".to_string(),
                upstream_api_key: "secret".to_string(),
                upstream_protocol: protocol,
                is_copilot: false,
                is_openrouter: false,
                is_starter: false,
                cors: false,
                timeout: 300,
                auth_token: None,
                aliases: HashMap::new(),
            }),
            client: http_utils::router_http_client(),
            key: test_key(),
            copilot_tokens: None,
            active_protocol: Arc::new(AtomicU8::new(protocol.to_u8())),
            log_store: LogStore::new(std::env::temp_dir()),
            logger: None,
            failover_keys: Arc::new(Vec::new()),
            shutdown: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn mock_reqwest_response(
        status: u16,
        content_type: &str,
        body: impl Into<String>,
    ) -> reqwest::Response {
        HttpResponse::builder()
            .status(status)
            .header("content-type", content_type)
            .body(body.into())
            .unwrap()
            .into()
    }

    #[test]
    fn convert_chat_response_for_responses_route_maps_buffered_json() {
        let chat = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "Hello from router"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });

        let response = convert_chat_response_for_responses_route(
            RouterResponse::buffered(200, CONTENT_TYPE_JSON, serde_json::to_vec(&chat).unwrap()),
            false,
            "gpt-4o",
        )
        .unwrap();

        match response {
            RouterResponse::Buffered {
                status,
                content_type,
                body,
            } => {
                let json: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(status, 200);
                assert_eq!(content_type, CONTENT_TYPE_JSON);
                assert_eq!(json["object"], "response");
                assert_eq!(json["output"][0]["content"][0]["text"], "Hello from router");
            }
            RouterResponse::Streaming { .. } => panic!("expected buffered response"),
        }
    }

    #[test]
    fn convert_chat_response_for_responses_route_maps_streaming_sse() {
        let chat_sse = concat!(
            "data: {\"id\":\"chatcmpl_1\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl_1\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl_1\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
            "data: [DONE]\n\n",
        );

        let response = convert_chat_response_for_responses_route(
            RouterResponse::buffered(200, "text/event-stream", chat_sse.as_bytes().to_vec()),
            true,
            "gpt-4o",
        )
        .unwrap();

        match response {
            RouterResponse::Buffered {
                status,
                content_type,
                body,
            } => {
                let sse = String::from_utf8(body).unwrap();
                assert_eq!(status, 200);
                assert_eq!(content_type, "text/event-stream");
                assert!(sse.contains("event: response.created"));
                assert!(sse.contains("\"delta\":\"Hel\""));
                assert!(sse.contains("event: response.completed"));
            }
            RouterResponse::Streaming { .. } => panic!("expected buffered SSE"),
        }
    }

    #[test]
    fn convert_chat_response_for_responses_route_rejects_streaming_non_stream_requests() {
        let response = convert_chat_response_for_responses_route(
            RouterResponse::Streaming {
                status: 200,
                content_type: "text/event-stream".to_string(),
                body: Box::new(StreamingBody::Upstream(mock_reqwest_response(
                    200,
                    "text/event-stream",
                    "data: [DONE]\n\n",
                ))),
            },
            false,
            "gpt-4o",
        );

        assert!(response.is_err());
    }

    #[test]
    fn responses_router_config_uses_active_protocol() {
        let state = test_state(ProviderProtocol::Google);
        let config = responses_router_config(&state);

        assert_eq!(config.target_protocol, ProviderProtocol::Google);
        assert_eq!(config.target_base_url, "https://example.com/v1");
        assert_eq!(config.api_key, "secret");
    }

    #[test]
    fn upstream_context_copies_router_flags() {
        let state = ServeState {
            config: Arc::new(ServeRouterConfig {
                upstream_base_url: "https://openrouter.ai/api/v1".to_string(),
                upstream_api_key: "secret".to_string(),
                upstream_protocol: ProviderProtocol::Openai,
                is_copilot: false,
                is_openrouter: true,
                is_starter: false,
                cors: false,
                timeout: 300,
                auth_token: None,
                aliases: HashMap::new(),
            }),
            client: http_utils::router_http_client(),
            key: test_key(),
            copilot_tokens: None,
            active_protocol: Arc::new(AtomicU8::new(ProviderProtocol::Openai.to_u8())),
            log_store: LogStore::new(std::env::temp_dir()),
            logger: None,
            failover_keys: Arc::new(Vec::new()),
            shutdown: Arc::new(tokio::sync::Notify::new()),
        };

        let context = upstream_context(&state);
        assert!(context.is_openrouter);
        assert!(!context.is_copilot);
        assert_eq!(context.upstream_base_url, "https://openrouter.ai/api/v1");
    }

    #[test]
    fn format_http_chunk_adds_hex_prefix_and_trailer() {
        assert_eq!(http_utils::format_http_chunk(b"hello"), b"5\r\nhello\r\n");
        assert!(http_utils::format_http_chunk(b"").is_empty());
    }

    #[test]
    fn convert_chat_response_for_responses_route_passes_error_status_through() {
        let error_body = br#"{"error":{"message":"rate limited"}}"#;
        let response = convert_chat_response_for_responses_route(
            RouterResponse::buffered(429, CONTENT_TYPE_JSON, error_body.to_vec()),
            false,
            "gpt-4o",
        )
        .unwrap();

        match response {
            RouterResponse::Buffered { status, body, .. } => {
                assert_eq!(status, 429);
                assert_eq!(body, error_body);
            }
            _ => panic!("expected buffered error passthrough"),
        }
    }

    #[test]
    fn convert_chat_response_for_responses_route_passes_500_through() {
        let error_body = br#"{"error":"internal"}"#;
        let response = convert_chat_response_for_responses_route(
            RouterResponse::buffered(500, CONTENT_TYPE_JSON, error_body.to_vec()),
            true,
            "gpt-4o",
        )
        .unwrap();

        match response {
            RouterResponse::Buffered { status, .. } => assert_eq!(status, 500),
            _ => panic!("expected buffered error passthrough"),
        }
    }

    #[test]
    fn format_http_chunk_large_payload() {
        let data = vec![b'x'; 256];
        let chunk = http_utils::format_http_chunk(&data);
        // 256 = 0x100
        assert!(chunk.starts_with(b"100\r\n"));
        assert!(chunk.ends_with(b"\r\n"));
    }

    #[test]
    fn format_http_chunk_single_byte() {
        let chunk = http_utils::format_http_chunk(b"a");
        assert_eq!(chunk, b"1\r\na\r\n");
    }

    #[test]
    fn responses_router_config_anthropic_protocol() {
        let state = test_state(ProviderProtocol::Anthropic);
        let config = responses_router_config(&state);
        assert_eq!(config.target_protocol, ProviderProtocol::Anthropic);
    }

    #[test]
    fn convert_chat_response_for_responses_route_buffered_json_to_stream() {
        let chat = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "streamed text"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2}
        });

        let response = convert_chat_response_for_responses_route(
            RouterResponse::buffered(200, CONTENT_TYPE_JSON, serde_json::to_vec(&chat).unwrap()),
            true, // client wants stream
            "gpt-4o",
        )
        .unwrap();

        match response {
            RouterResponse::Buffered {
                status,
                content_type,
                body,
            } => {
                assert_eq!(status, 200);
                assert_eq!(content_type, "text/event-stream");
                let sse = String::from_utf8(body).unwrap();
                assert!(sse.contains("event: response.created"));
                assert!(sse.contains("streamed text"));
                assert!(sse.contains("event: response.completed"));
            }
            RouterResponse::Streaming { .. } => panic!("expected buffered SSE"),
        }
    }

    // ── Failover tests ────────────────────────────────────────────────────

    #[test]
    fn is_failover_status_triggers_on_auth_errors() {
        assert!(is_failover_status(401));
        assert!(is_failover_status(403));
    }

    #[test]
    fn is_failover_status_triggers_on_rate_limit() {
        assert!(is_failover_status(429));
    }

    #[test]
    fn is_failover_status_triggers_on_server_errors() {
        assert!(is_failover_status(500));
        assert!(is_failover_status(502));
        assert!(is_failover_status(503));
        assert!(is_failover_status(504));
        assert!(is_failover_status(599));
    }

    #[test]
    fn is_failover_status_does_not_trigger_on_success() {
        assert!(!is_failover_status(200));
        assert!(!is_failover_status(201));
        assert!(!is_failover_status(204));
    }

    #[test]
    fn is_failover_status_does_not_trigger_on_client_errors() {
        // Client errors that indicate a bad request — retrying with a different
        // key won't help.
        assert!(!is_failover_status(400));
        assert!(!is_failover_status(404));
        assert!(!is_failover_status(405));
        assert!(!is_failover_status(422));
    }

    #[test]
    fn failover_state_builds_from_entry() {
        let entry = FailoverEntry {
            config: Arc::new(ServeRouterConfig {
                upstream_base_url: "https://backup.example.com/v1".to_string(),
                upstream_api_key: "backup-key".to_string(),
                upstream_protocol: ProviderProtocol::Openai,
                is_copilot: false,
                is_openrouter: false,
                is_starter: false,
                cors: false,
                timeout: 300,
                auth_token: None,
                aliases: HashMap::new(),
            }),
            key: test_key(),
            copilot_tokens: None,
            active_protocol: AtomicU8::new(ProviderProtocol::Openai.to_u8()),
        };

        let client = http_utils::router_http_client();
        let state = failover_state(&entry, &client, &LogStore::new(std::env::temp_dir()));

        assert_eq!(
            state.config.upstream_base_url,
            "https://backup.example.com/v1"
        );
        assert_eq!(state.config.upstream_api_key, "backup-key");
        assert!(state.logger.is_none());
        assert!(state.failover_keys.is_empty());
    }

    #[test]
    fn failover_state_shares_arc_config() {
        let config = Arc::new(ServeRouterConfig {
            upstream_base_url: "https://backup.example.com/v1".to_string(),
            upstream_api_key: "key".to_string(),
            upstream_protocol: ProviderProtocol::Openai,
            is_copilot: false,
            is_openrouter: false,
            is_starter: false,
            cors: false,
            timeout: 300,
            auth_token: None,
            aliases: HashMap::new(),
        });

        let entry = FailoverEntry {
            config: config.clone(),
            key: test_key(),
            copilot_tokens: None,
            active_protocol: AtomicU8::new(ProviderProtocol::Openai.to_u8()),
        };

        let client = http_utils::router_http_client();
        let state = failover_state(&entry, &client, &LogStore::new(std::env::temp_dir()));

        // Arc should be a clone of the same allocation, not a new copy
        assert!(Arc::ptr_eq(&entry.config, &state.config));
    }

    #[test]
    fn failover_state_does_not_cascade() {
        let entry = FailoverEntry {
            config: Arc::new(ServeRouterConfig {
                upstream_base_url: "https://backup.example.com/v1".to_string(),
                upstream_api_key: "key".to_string(),
                upstream_protocol: ProviderProtocol::Openai,
                is_copilot: false,
                is_openrouter: false,
                is_starter: false,
                cors: false,
                timeout: 300,
                auth_token: None,
                aliases: HashMap::new(),
            }),
            key: test_key(),
            copilot_tokens: None,
            active_protocol: AtomicU8::new(ProviderProtocol::Openai.to_u8()),
        };

        let client = http_utils::router_http_client();
        let state = failover_state(&entry, &client, &LogStore::new(std::env::temp_dir()));

        // Failover state should have no failover keys (no cascading)
        assert!(state.failover_keys.is_empty());
        // Logger should be disabled on failover attempts
        assert!(state.logger.is_none());
    }

    #[test]
    fn health_response_is_valid_json() {
        let json: Value = serde_json::from_slice(&HEALTH_RESPONSE).unwrap();
        assert_eq!(json["status"], "ok");
        assert!(json["version"].is_string());
    }

    #[test]
    fn health_response_is_stable() {
        // LazyLock should return the same bytes every time
        let a = HEALTH_RESPONSE.clone();
        let b = HEALTH_RESPONSE.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn is_failover_status_boundary_499() {
        // 499 is a non-standard client error — should NOT trigger failover
        assert!(!is_failover_status(499));
    }

    #[test]
    fn is_failover_status_boundary_600() {
        // 600 is outside the 5xx range — should NOT trigger failover
        assert!(!is_failover_status(600));
    }

    #[test]
    fn convert_chat_response_for_responses_route_malformed_json_body() {
        // A non-JSON body with status 200 should fail to parse and return an error
        let response = convert_chat_response_for_responses_route(
            RouterResponse::buffered(200, CONTENT_TYPE_JSON, b"not valid json".to_vec()),
            false,
            "gpt-4o",
        );
        assert!(response.is_err());
    }

    #[test]
    fn convert_chat_response_for_responses_route_empty_body_non_stream() {
        // An empty body with status 200 should fail to parse and return an error
        let response = convert_chat_response_for_responses_route(
            RouterResponse::buffered(200, CONTENT_TYPE_JSON, Vec::new()),
            false,
            "gpt-4o",
        );
        assert!(response.is_err());
    }

    #[test]
    fn convert_chat_response_for_responses_route_error_stream_passthrough() {
        // A 400 error response passes through unchanged even when client wants stream
        let error_body = br#"{"error":{"message":"bad request"}}"#;
        let response = convert_chat_response_for_responses_route(
            RouterResponse::buffered(400, CONTENT_TYPE_JSON, error_body.to_vec()),
            true,
            "gpt-4o",
        )
        .unwrap();

        match response {
            RouterResponse::Buffered { status, body, .. } => {
                assert_eq!(status, 400);
                assert_eq!(body, error_body);
            }
            _ => panic!("expected buffered error passthrough"),
        }
    }

    #[test]
    fn responses_router_config_openai_protocol() {
        let state = test_state(ProviderProtocol::Openai);
        let config = responses_router_config(&state);

        assert_eq!(config.target_protocol, ProviderProtocol::Openai);
        assert_eq!(config.target_base_url, "https://example.com/v1");
        assert_eq!(config.api_key, "secret");
        assert!(config.copilot_token_manager.is_none());
        assert!(config.model_prefix.is_none());
        assert!(!config.requires_reasoning_content);
        assert!(config.actual_model.is_none());
        assert!(config.max_tokens_cap.is_none());
        assert!(config.responses_api_supported.is_none());
    }

    #[test]
    fn apply_alias_rewrites_known_alias() {
        let aliases = HashMap::from([("fast".to_string(), "gpt-4o-mini".to_string())]);
        let mut body = json!({"model": "fast", "messages": []});
        apply_alias(&mut body, &aliases);
        assert_eq!(body["model"], "gpt-4o-mini");
    }

    #[test]
    fn apply_alias_passes_through_unknown_model() {
        let aliases = HashMap::from([("fast".to_string(), "gpt-4o-mini".to_string())]);
        let mut body = json!({"model": "claude-sonnet-4-6", "messages": []});
        apply_alias(&mut body, &aliases);
        assert_eq!(body["model"], "claude-sonnet-4-6");
    }

    #[test]
    fn apply_alias_follows_chain() {
        let aliases = HashMap::from([
            ("fast".to_string(), "quick".to_string()),
            ("quick".to_string(), "gpt-4o-mini".to_string()),
        ]);
        let mut body = json!({"model": "fast"});
        apply_alias(&mut body, &aliases);
        assert_eq!(body["model"], "gpt-4o-mini");
    }

    #[test]
    fn apply_alias_no_op_when_field_missing_or_empty() {
        let aliases = HashMap::from([("fast".to_string(), "gpt-4o-mini".to_string())]);

        let mut body = json!({"messages": []});
        apply_alias(&mut body, &aliases);
        assert!(body.get("model").is_none());

        let mut body = json!({"model": "", "messages": []});
        apply_alias(&mut body, &aliases);
        assert_eq!(body["model"], "");
    }

    #[test]
    fn apply_alias_no_op_when_alias_map_empty() {
        let mut body = json!({"model": "fast"});
        apply_alias(&mut body, &HashMap::new());
        assert_eq!(body["model"], "fast");
    }
}
