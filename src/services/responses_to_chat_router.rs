//! Responses-to-Chat router service
//!
//! Acts as an HTTP proxy that accepts OpenAI Responses API requests and forwards
//! them to upstreams that may only support Chat Completions or other protocols.
//!
//! 1. Tool filtering: strips built-in tool types (computer_use, file_search,
//!    web_search, code_interpreter) that most non-OpenAI providers reject.
//!
//! 2. Responses API conversion: clients like Codex CLI use `/v1/responses`
//!    with `input` items. Providers that only support `/v1/chat/completions`
//!    need a full request/response conversion. This router handles that automatically.
//!
//! Conversion logic (Responses API ↔ Chat Completions) lives in
//! `responses_chat_conversion.rs` and is re-exported here for backwards compatibility.
use crate::constants::CONTENT_TYPE_JSON;
use crate::services::anthropic_chat_request::ensure_assistant_reasoning_content_in_chat_request;
use crate::services::anthropic_route_pipeline::inject_chat_completions_cache_control;
use crate::services::copilot_auth::CopilotTokenManager;
use crate::services::device_fingerprint;
use crate::services::http_debug::LoggedSend;
use crate::services::http_utils::{self};
use crate::services::model_names::select_model_for_provider_attempt;
use crate::services::openai_anthropic_bridge::{
    OpenAIToAnthropicChatConfig, convert_anthropic_to_openai_chat_response,
    convert_openai_chat_response_to_sse, convert_openai_chat_to_anthropic_request,
};
use crate::services::openai_gemini_bridge::{
    OpenAIToGeminiConfig, build_google_generate_content_url,
    convert_gemini_to_openai_chat_response, convert_openai_chat_to_gemini_request,
    openai_chat_model,
};
use crate::services::protocol_fallback::{
    AttemptOutcome, classify_attempt, commit_protocol_switch, protocol_candidates,
    record_request_outcome,
};
use crate::services::provider_profile::is_openrouter_base;
use crate::services::provider_protocol::{
    PathVariant, ProviderProtocol, classify_failed_attempt, decode_route, encode_route,
    is_endpoint_missing,
};
use crate::services::responses_chat_conversion;
use crate::services::route_cache::{PersistedRoute, RouteCache, RouteSlot};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// Re-export public conversion functions used by other modules
pub use responses_chat_conversion::{
    convert_chat_response_to_responses_sse, convert_responses_to_chat_request,
    is_responses_api_format, parse_provider_response,
};

// Internal re-exports used within this router
use responses_chat_conversion::{
    ResponsesStreamConverter, apply_max_tokens_cap_to_fields, cap_reasoning_effort,
    sanitize_input_content,
};

#[derive(Clone)]
pub struct ResponsesToChatRouterConfig {
    pub target_base_url: String,
    pub api_key: String,
    pub target_protocol: ProviderProtocol,
    /// Persisted path-variant pin from a prior launch. When set, the router
    /// skips re-probing the alternate variant.
    pub target_path_variant: Option<PathVariant>,
    pub copilot_token_manager: Option<Arc<CopilotTokenManager>>,
    /// Optional model prefix to add (e.g., "@cf/" for Cloudflare)
    pub model_prefix: Option<String>,
    /// Whether the provider requires a non-empty `reasoning_content` sentinel on assistant
    /// tool-call turns even when the provider returned no reasoning text (e.g., Moonshot).
    /// Auto-detection handles the normal case: if the provider returns `reasoning_content`
    /// in a response it is always round-tripped, regardless of this flag.
    pub requires_reasoning_content: bool,
    /// The actual model name to use with the provider (e.g., "kimi-k2.5" while Codex CLI sees "gpt-4o")
    pub actual_model: Option<String>,
    /// Cap applied to `max_tokens` / `max_output_tokens` before forwarding to the provider,
    /// for providers that reject values above a fixed output ceiling.
    pub max_tokens_cap: Option<u64>,
    /// Persisted Responses API support state: None = unknown, Some(true) = supported,
    /// Some(false) = not supported.  Avoids a wasted probe request on every session.
    pub responses_api_supported: Option<bool>,
    /// Whether this is the aivo starter provider (requires device fingerprint headers).
    pub is_starter: bool,
    /// Bare model ids whose upstream catalog entry has an `aivo/` prefix the
    /// caller's SDK strips (e.g. opencode's `@ai-sdk/openai-compatible`
    /// strips the provider name `aivo/` so `aivo/starter` arrives as
    /// `starter`). When the body's outgoing model matches an entry here,
    /// the router prepends `aivo/` so the upstream resolves the alias.
    pub aivo_prefix_models: Vec<String>,
}

pub struct ResponsesToChatRouter {
    config: ResponsesToChatRouterConfig,
    /// `(tool, key, model)` namespace this router persists under — the same
    /// router serves codex/opencode/pi.
    tool: &'static str,
    /// Learned per-model seed for the `RouteCache`. On the router (not the
    /// config) so the conversion-test config literals don't need a new field.
    seed_routes: BTreeMap<String, PersistedRoute>,
}

enum ForwardedChatResponse {
    Success(Value),
    HttpError { status: u16, body: String },
}

struct ResponsesToChatRouterState {
    config: Arc<ResponsesToChatRouterConfig>,
    client: Arc<reqwest::Client>,
    /// Per-model learned routes; the cascade reads/writes the resolved slot's
    /// atom and `slot.confirm()` marks authoritative outcomes for write-behind.
    /// `ResponsesApi` on a slot = native `/v1/responses`; `Openai` = chat.
    route_cache: Arc<RouteCache>,
    /// Flipped to `true` when an upstream returns an error envelope matching
    /// the `requires_reasoning_content` quirk. Persisted to `ApiKey` so future
    /// launches enable strict mode without hardcoding the host.
    learned_requires_reasoning: Arc<AtomicBool>,
}

impl ResponsesToChatRouter {
    pub fn new(config: ResponsesToChatRouterConfig) -> Self {
        Self {
            config,
            tool: "codex",
            seed_routes: BTreeMap::new(),
        }
    }

    /// Stamp the `(tool, key, model)` namespace ("codex" | "opencode" | "pi").
    pub fn with_tool(mut self, tool: &'static str) -> Self {
        self.tool = tool;
        self
    }

    pub fn with_seed_routes(mut self, seed_routes: BTreeMap<String, PersistedRoute>) -> Self {
        self.seed_routes = seed_routes;
        self
    }

    /// Per-model seed: learned routes plus a `""` default (the configured /
    /// detected protocol) so a migrated `codexResponsesApi` informs request #1.
    fn build_seed(&self) -> BTreeMap<String, PersistedRoute> {
        let mut seed = self.seed_routes.clone();
        seed.entry(String::new()).or_insert_with(|| {
            // Codex is responses-native → default to ResponsesApi (probe
            // /v1/responses first). Known chat-only keys (codexResponsesApi=false)
            // and non-codex tools (opencode/pi are chat clients) keep chat.
            let proto = match (self.tool, self.config.responses_api_supported) {
                ("codex", Some(false)) => self.config.target_protocol,
                ("codex", _) => ProviderProtocol::ResponsesApi,
                _ => self.config.target_protocol,
            };
            PersistedRoute::from_route(
                proto,
                self.config
                    .target_path_variant
                    .unwrap_or(PathVariant::Default),
            )
        });
        seed
    }

    /// Binds to a random available port and starts the router in the background.
    /// Returns the actual port number so callers can set OPENAI_BASE_URL.
    pub async fn start_background(
        &self,
    ) -> Result<(
        u16,
        Arc<RouteCache>,
        Arc<AtomicBool>,
        tokio::task::JoinHandle<Result<()>>,
    )> {
        let (listener, port) = http_utils::bind_local_listener().await?;
        // Tool-native protocol for the codex family: OpenAI chat first; native
        // `/v1/responses` is the learned fallback.
        let route_cache = Arc::new(RouteCache::new(
            self.tool,
            ProviderProtocol::Openai,
            self.build_seed(),
        ));
        let learned_requires_reasoning = Arc::new(AtomicBool::new(false));
        let state = ResponsesToChatRouterState {
            config: Arc::new(self.config.clone()),
            client: Arc::new(http_utils::router_http_client()),
            route_cache: route_cache.clone(),
            learned_requires_reasoning: learned_requires_reasoning.clone(),
        };
        let handle = tokio::spawn(async move {
            http_utils::run_streaming_router(
                listener,
                Arc::new(state),
                handle_router_request_streaming,
            )
            .await
        });
        Ok((port, route_cache, learned_requires_reasoning, handle))
    }
}

pub(crate) async fn forward_chat_completions_with_fallback(
    body: &Value,
    config: ResponsesToChatRouterConfig,
    client: &reqwest::Client,
    force_non_streaming: bool,
) -> Result<(Value, ProviderProtocol)> {
    // One-shot (non-router) path: a single throwaway cache slot seeded from the
    // configured default route lets us reuse the cascade unchanged.
    let mut seed = BTreeMap::new();
    seed.insert(
        String::new(),
        PersistedRoute::from_route(
            config.target_protocol,
            config.target_path_variant.unwrap_or(PathVariant::Default),
        ),
    );
    let cache = RouteCache::new("codex", config.target_protocol, seed);
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    let slot = cache.resolve(model);
    let learned_requires_reasoning = Arc::new(AtomicBool::new(false));
    let config = Arc::new(config);

    match forward_openai_chat_request(
        body,
        &config,
        client,
        force_non_streaming,
        &slot,
        &learned_requires_reasoning,
    )
    .await?
    {
        ForwardedChatResponse::Success(value) => {
            let (protocol, _) = slot.current();
            Ok((value, protocol))
        }
        ForwardedChatResponse::HttpError { status, body } => {
            let status = reqwest::StatusCode::from_u16(status)
                .map(|s| s.to_string())
                .unwrap_or_else(|_| status.to_string());
            anyhow::bail!("API returned {} — {}", status, body)
        }
    }
}

async fn handle_router_request_streaming(
    request: String,
    state: Arc<ResponsesToChatRouterState>,
    mut socket: tokio::net::TcpStream,
) {
    use tokio::io::AsyncWriteExt;
    if let Some(response) = handle_router_request(request, &state, &mut socket).await {
        let _ = socket.write_all(response.as_bytes()).await;
    }
}

/// Quick check on a buffered HTTP/1.1 response string: status starts at byte
/// offset 9 (after "HTTP/1.1 "). 2xx → success.
fn response_is_2xx(http_response: &str) -> bool {
    http_response
        .as_bytes()
        .get(9..12)
        .and_then(|b| std::str::from_utf8(b).ok())
        .and_then(|s| s.parse::<u16>().ok())
        .is_some_and(|status| (200..300).contains(&status))
}

/// Returns `Some(response)` for buffered responses, `None` if the handler
/// already streamed the response directly to the socket.
async fn handle_router_request(
    request: String,
    state: &ResponsesToChatRouterState,
    socket: &mut tokio::net::TcpStream,
) -> Option<String> {
    let path = http_utils::extract_request_path(&request);

    let is_api_path = matches!(
        path.as_str(),
        "/responses" | "/v1/responses" | "/chat/completions" | "/v1/chat/completions"
    );

    if is_api_path {
        // Resolve this request's per-model route slot up front so the cascade
        // and the failure-streak accounting below share one atom.
        let model = http_utils::extract_request_body(&request)
            .ok()
            .and_then(|b| serde_json::from_str::<Value>(b).ok())
            .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_string))
            .unwrap_or_default();
        let slot = state.route_cache.resolve(&model);
        let response = match handle_api_request(
            &path,
            &request,
            &state.config,
            state.client.as_ref(),
            &slot,
            &state.learned_requires_reasoning,
            socket,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => Some(http_utils::http_error_response(
                500,
                &format!("Internal Server Error: {e:#}"),
            )),
        };
        // Failure-streak demotion for buffered responses; streamed responses
        // (None) already recorded success via `slot.confirm()` inside the
        // streamer.
        if let Some(resp) = &response {
            let (seed_proto, seed_variant) = slot.seed_route();
            record_request_outcome(
                slot.route_atom(),
                slot.failures_atom(),
                seed_proto,
                seed_variant,
                response_is_2xx(resp),
            );
        }
        response
    } else {
        match forward_request(&path, &request, &state.config, state.client.as_ref()).await {
            Ok(r) => Some(r),
            Err(e) => Some(http_utils::http_error_response(
                502,
                &format!("Bad Gateway: {e:#}"),
            )),
        }
    }
}

/// Before bailing a streaming attempt on a non-200, peek the error body and
/// learn the `requires_reasoning_content` quirk so the buffered fallback's
/// first attempt is already strict instead of eating a second 400. Consumes
/// the response (we're bailing regardless).
async fn note_streaming_failure(
    response: reqwest::Response,
    learned_requires_reasoning: &AtomicBool,
) {
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    if classify_failed_attempt(status, &body).quirk_hint == Some("requires_reasoning_content") {
        learned_requires_reasoning.store(true, Ordering::Relaxed);
    }
}

/// Routes the request based on body format:
/// - Responses API format (has "input" array): convert ↔ Chat Completions
/// - Chat Completions format: filter non-function tools and forward
///
/// Returns `None` if the response was streamed directly to the socket.
async fn handle_api_request(
    path: &str,
    request: &str,
    config: &Arc<ResponsesToChatRouterConfig>,
    client: &reqwest::Client,
    slot: &RouteSlot,
    learned_requires_reasoning: &Arc<AtomicBool>,
    socket: &mut tokio::net::TcpStream,
) -> Result<Option<String>> {
    let body_str = http_utils::extract_request_body(request)?;
    let body: Value = serde_json::from_str(body_str)?;

    if is_responses_api_format(&body) {
        // Responses-first: probe /v1/responses while the route is ResponsesApi
        // (prior or learned). On miss, demote to chat so the cascade pins
        // openai/anthropic and the next launch won't re-probe responses.
        if slot.current().0 == ProviderProtocol::ResponsesApi {
            if let Some(result) =
                try_responses_api_passthrough(&body, config, client, slot, socket).await
            {
                return result;
            }
            slot.route_atom().store(
                encode_route(ProviderProtocol::Openai, PathVariant::Default),
                Ordering::Relaxed,
            );
        }
        // Chat fallback: stream the converted request first; on any pre-stream
        // failure use the buffered cascade (which can escalate to /v1/messages).
        if stream_responses_via_chat(
            &body,
            config,
            client,
            slot,
            learned_requires_reasoning,
            socket,
        )
        .await
        .is_ok()
        {
            return Ok(None); // already streamed to socket
        }
        let response = handle_responses_api_via_chat(
            path,
            &body,
            config,
            client,
            slot,
            learned_requires_reasoning,
        )
        .await?;
        Ok(Some(response))
    } else {
        // For streaming Chat Completions, stream directly from upstream to client
        if body
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            && stream_chat_completions(
                &body,
                config,
                client,
                slot,
                learned_requires_reasoning,
                socket,
            )
            .await
            .is_ok()
        {
            return Ok(None); // already streamed to socket
        }
        Ok(Some(
            handle_chat_completions_with_filter(
                path,
                &body,
                config,
                client,
                slot,
                learned_requires_reasoning,
            )
            .await?,
        ))
    }
}

// =============================================================================
// RESPONSES API PATH: passthrough or convert
// =============================================================================

/// Forward a Responses request to the upstream `/v1/responses`.
///
/// Return shape:
/// - `Some(Ok(Some(response)))` — buffered HTTP response the caller writes.
/// - `Some(Ok(None))` — already streamed chunked SSE to `socket`.
/// - `Some(Err(_))` — surfaced to the caller.
/// - `None` — endpoint missing / wrong shape; fall back to Chat Completions.
async fn try_responses_api_passthrough(
    body: &Value,
    config: &Arc<ResponsesToChatRouterConfig>,
    client: &reqwest::Client,
    slot: &RouteSlot,
    socket: &mut tokio::net::TcpStream,
) -> Option<Result<Option<String>>> {
    let variant = slot.current().1;
    // Confirmed-responses routes surface their errors; an unprobed (prior-only)
    // route falls back to chat on any miss so a blip isn't taken as the answer.
    let committed = slot.is_confirmed();

    let mut body = body.clone();
    // Don't filter_tools here — the upstream Responses API supports all tool types
    // (computer_use_preview, web_search_preview, code_interpreter, etc.).
    // Tool filtering is only needed for the Chat Completions conversion path.

    // Strip Chat Completions-only parameters that the Responses API doesn't accept
    if let Some(obj) = body.as_object_mut() {
        obj.remove("stream_options");
    }
    // Cap reasoning effort: xhigh is not supported by most models
    cap_reasoning_effort(&mut body);
    // Ensure all message content parts have a `text` field — the Responses API
    // rejects `output_text` / `input_text` parts that are missing it.
    sanitize_input_content(&mut body);
    apply_max_tokens_cap_to_fields(&mut body, config.max_tokens_cap, &["max_output_tokens"]);
    apply_selected_model(&mut body, config.as_ref(), ProviderProtocol::Openai);

    let target_url = build_target_url(&config.target_base_url, variant.apply("/v1/responses"));
    let req = http_utils::authorized_openai_post(
        client,
        &target_url,
        &config.api_key,
        config.copilot_token_manager.as_deref(),
        None,
    )
    .await
    .ok()?;
    let mut response =
        device_fingerprint::maybe_with_starter_headers(req.json(&body), config.is_starter)
            .send_logged()
            .await
            .ok()?;

    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(CONTENT_TYPE_JSON)
        .to_string();
    let is_sse = content_type.contains("text/event-stream");

    if status != 200 {
        let response_body = response.text().await.ok()?;
        // A committed (learned) Responses route surfaces its real errors; an
        // uncommitted probe falls back to chat on anything non-200.
        if committed && !is_endpoint_missing(status) {
            return Some(Ok(Some(http_utils::http_response(
                status,
                &content_type,
                &response_body,
            ))));
        }
        return None;
    }

    if is_sse {
        // Stream the SSE body chunk-by-chunk to the client. Previously this
        // function drained the entire stream with `.text().await` and replayed
        // it under Content-Length, which froze codex's UI until upstream
        // produced `response.completed` — turning a live stream into a single
        // blob at the end of the turn.
        let mut prefix: Vec<u8> = Vec::new();
        if !committed {
            // First-probe validation: read until we see a Responses-API event
            // signature, or until 4 KiB without one (latch unsupported). We
            // must not write headers to the socket before we know the upstream
            // is the right shape, since a wrong-shape response must fall back
            // to the chat-completions conversion path.
            const SIGNATURE: &str = "event: response.";
            const SCAN_LIMIT: usize = 4096;
            let mut validated = false;
            loop {
                let chunk = match response.chunk().await {
                    Ok(Some(c)) => c,
                    Ok(None) => break,
                    Err(_) => return None,
                };
                prefix.extend_from_slice(&chunk);
                // Search raw bytes: the signature is ASCII, and a multi-byte
                // UTF-8 char split across a chunk boundary would make a
                // str::from_utf8 check spuriously skip a round.
                if prefix
                    .windows(SIGNATURE.len())
                    .any(|w| w == SIGNATURE.as_bytes())
                {
                    validated = true;
                    break;
                }
                if prefix.len() >= SCAN_LIMIT {
                    break;
                }
            }
            if !validated {
                return None;
            }
        }
        // Native /v1/responses works for this model — pin it so the next
        // request prefers the passthrough, and mark it proven for write-behind.
        slot.route_atom().store(
            encode_route(ProviderProtocol::ResponsesApi, variant),
            Ordering::Relaxed,
        );
        slot.confirm();
        match http_utils::write_streaming_response_with_prefix(
            socket,
            status,
            &content_type,
            &prefix,
            response,
        )
        .await
        {
            Ok(()) => Some(Ok(None)),
            Err(e) => Some(Err(e)),
        }
    } else {
        // Non-streaming JSON response — keep the buffered path so the
        // shape-validation step can inspect the full body.
        let response_body = response.text().await.ok()?;
        if !committed {
            let looks_like_responses_api = response_body.contains("\"object\":\"response\"")
                || response_body.contains("\"object\": \"response\"");
            if !looks_like_responses_api {
                return None;
            }
        }
        slot.route_atom().store(
            encode_route(ProviderProtocol::ResponsesApi, variant),
            Ordering::Relaxed,
        );
        slot.confirm();
        Some(Ok(Some(http_utils::http_response(
            status,
            &content_type,
            &response_body,
        ))))
    }
}

/// Handles Responses API requests by converting to Chat Completions format,
/// forwarding to the provider, and converting the response back to Responses
/// API SSE format that the Codex CLI expects.
async fn handle_responses_api_via_chat(
    _path: &str,
    body: &Value,
    config: &Arc<ResponsesToChatRouterConfig>,
    client: &reqwest::Client,
    slot: &RouteSlot,
    learned_requires_reasoning: &Arc<AtomicBool>,
) -> Result<String> {
    // Extract original model before conversion
    let original_model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("gpt-4o")
        .to_string();

    // Create a config copy with the model pinned to avoid protocol-based transformation
    // before we know which protocol the fallback loop will select. Flip on the
    // learned strict-mode quirk here so requests after a successful in-cascade
    // recovery skip the wasted first attempt — without this, every request in
    // the same launch pays one 400 + retry round-trip until process exit.
    let mut chat_config = (**config).clone();
    // Don't clobber a configured upstream model with the client's display name.
    if chat_config.actual_model.is_none() {
        chat_config.actual_model = Some(original_model.clone());
    }
    chat_config.requires_reasoning_content =
        config.requires_reasoning_content || learned_requires_reasoning.load(Ordering::Relaxed);
    let chat_body = convert_responses_to_chat_request(body, &chat_config);
    let chat_response = match forward_openai_chat_request(
        &chat_body,
        config,
        client,
        false,
        slot,
        learned_requires_reasoning,
    )
    .await?
    {
        ForwardedChatResponse::Success(value) => value,
        ForwardedChatResponse::HttpError { status, body } => {
            return Ok(http_utils::http_json_response(status, &body));
        }
    };
    let sse = convert_chat_response_to_responses_sse(
        &chat_response,
        config.requires_reasoning_content,
        &original_model,
    );

    Ok(http_utils::http_response(200, "text/event-stream", &sse))
}

/// Streaming counterpart of `handle_responses_api_via_chat`: forwards a
/// `stream: true` Chat Completions request and converts each delta into
/// Responses API SSE events written straight to the client socket.
///
/// Only runs when the route is already settled on the OpenAI protocol (the
/// common case for OpenAI-compatible providers like DeepSeek). Bails before
/// writing any bytes on a non-200 / non-SSE upstream so the caller can fall
/// back to the buffered conversion cascade, which keeps the protocol-probe and
/// strict-mode retry behavior.
async fn stream_responses_via_chat(
    body: &Value,
    config: &Arc<ResponsesToChatRouterConfig>,
    client: &reqwest::Client,
    slot: &RouteSlot,
    learned_requires_reasoning: &Arc<AtomicBool>,
    socket: &mut tokio::net::TcpStream,
) -> Result<()> {
    let (protocol, variant) = slot.current();
    // Openai and ResponsesApi both hit /v1/chat/completions; codex pins
    // ResponsesApi, so gating on Openai alone dropped every turn to buffered.
    if !matches!(
        protocol,
        ProviderProtocol::Openai | ProviderProtocol::ResponsesApi
    ) {
        anyhow::bail!("streaming conversion only for OpenAI-compatible protocols");
    }

    let original_model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("gpt-4o")
        .to_string();
    let effective_requires_reasoning =
        config.requires_reasoning_content || learned_requires_reasoning.load(Ordering::Relaxed);

    // Mirror handle_responses_api_via_chat's body construction, then force a
    // streaming request and ask for a trailing usage chunk.
    let mut chat_config = (**config).clone();
    // Don't clobber a configured upstream model with the client's display name.
    if chat_config.actual_model.is_none() {
        chat_config.actual_model = Some(original_model.clone());
    }
    chat_config.requires_reasoning_content = effective_requires_reasoning;
    let mut chat_body = convert_responses_to_chat_request(body, &chat_config);
    chat_body["stream"] = json!(true);
    chat_body["stream_options"] = json!({"include_usage": true});

    let target_url = build_target_url(
        &config.target_base_url,
        variant.apply("/v1/chat/completions"),
    );
    let initiator = if config.copilot_token_manager.is_some() {
        Some(http_utils::copilot_initiator_from_openai(&chat_body))
    } else {
        None
    };
    let req = http_utils::authorized_openai_post(
        client,
        &target_url,
        &config.api_key,
        config.copilot_token_manager.as_deref(),
        initiator,
    )
    .await?;
    let mut response =
        device_fingerprint::maybe_with_starter_headers(req.json(&chat_body), config.is_starter)
            .send_logged()
            .await?;

    let status = response.status().as_u16();
    if status != 200 {
        note_streaming_failure(response, learned_requires_reasoning).await;
        anyhow::bail!("upstream returned {status}");
    }
    let is_sse = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(false);
    if !is_sse {
        anyhow::bail!("upstream did not return an SSE stream");
    }

    slot.confirm();

    use tokio::io::AsyncWriteExt;
    let headers = http_utils::http_chunked_response_head(200, "text/event-stream");
    socket.write_all(headers.as_bytes()).await?;
    let mut converter =
        ResponsesStreamConverter::new(&original_model, effective_requires_reasoning);
    while let Some(chunk) = response.chunk().await? {
        let converted = converter.push_bytes(&chunk);
        if !converted.is_empty() {
            socket
                .write_all(&http_utils::format_http_chunk(converted.as_bytes()))
                .await?;
        }
    }
    let tail = converter.finish();
    if !tail.is_empty() {
        socket
            .write_all(&http_utils::format_http_chunk(tail.as_bytes()))
            .await?;
    }
    socket.write_all(b"0\r\n\r\n").await?;
    Ok(())
}

// =============================================================================
// CHAT COMPLETIONS PATH: streaming passthrough
// =============================================================================

/// Applies shared request transforms (tool filtering, token caps, model selection).
/// `requires_reasoning_content` is passed in (rather than read from `config`) so
/// callers can OR in the runtime-learned flag — letting requests after a
/// successful in-cascade recovery skip the wasted first attempt.
fn prepare_chat_completions_body(
    body: &Value,
    config: &ResponsesToChatRouterConfig,
    protocol: ProviderProtocol,
    requires_reasoning_content: bool,
) -> Value {
    let mut body = body.clone();
    filter_tools(&mut body);
    apply_max_tokens_cap_to_fields(
        &mut body,
        config.max_tokens_cap,
        &["max_tokens", "max_output_tokens"],
    );
    if requires_reasoning_content {
        ensure_assistant_reasoning_content_in_chat_request(&mut body);
    }
    apply_selected_model(&mut body, config, protocol);
    body
}

/// Streams a Chat Completions request directly from upstream to the client socket,
/// forwarding SSE chunks in real time so reasoning_content appears progressively.
async fn stream_chat_completions(
    body: &Value,
    config: &Arc<ResponsesToChatRouterConfig>,
    client: &reqwest::Client,
    slot: &RouteSlot,
    learned_requires_reasoning: &Arc<AtomicBool>,
    socket: &mut tokio::net::TcpStream,
) -> Result<()> {
    // Only stream for OpenAI protocol (the common case for DeepSeek, etc.)
    let (protocol, variant) = slot.current();
    if protocol != ProviderProtocol::Openai {
        anyhow::bail!("streaming passthrough only for OpenAI protocol");
    }

    let effective_requires_reasoning =
        config.requires_reasoning_content || learned_requires_reasoning.load(Ordering::Relaxed);
    let body = prepare_chat_completions_body(body, config, protocol, effective_requires_reasoning);

    let target_url = build_target_url(
        &config.target_base_url,
        variant.apply("/v1/chat/completions"),
    );
    let initiator = if config.copilot_token_manager.is_some() {
        Some(http_utils::copilot_initiator_from_openai(&body))
    } else {
        None
    };
    let req = http_utils::authorized_openai_post(
        client,
        &target_url,
        &config.api_key,
        config.copilot_token_manager.as_deref(),
        initiator,
    )
    .await?;
    let response =
        device_fingerprint::maybe_with_starter_headers(req.json(&body), config.is_starter)
            .send_logged()
            .await?;

    let status = response.status().as_u16();
    if status != 200 {
        note_streaming_failure(response, learned_requires_reasoning).await;
        anyhow::bail!("upstream returned {status}");
    }

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/event-stream")
        .to_string();

    slot.confirm();
    http_utils::write_streaming_response(socket, 200, &content_type, response).await
}

// =============================================================================
// CHAT COMPLETIONS PATH: filter tools and forward (buffered)
// =============================================================================

async fn handle_chat_completions_with_filter(
    _path: &str,
    body: &Value,
    config: &Arc<ResponsesToChatRouterConfig>,
    client: &reqwest::Client,
    slot: &RouteSlot,
    learned_requires_reasoning: &Arc<AtomicBool>,
) -> Result<String> {
    let effective_requires_reasoning =
        config.requires_reasoning_content || learned_requires_reasoning.load(Ordering::Relaxed);
    let body =
        prepare_chat_completions_body(body, config, slot.current().0, effective_requires_reasoning);
    let requested_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let chat_response = match forward_openai_chat_request(
        &body,
        config,
        client,
        requested_stream,
        slot,
        learned_requires_reasoning,
    )
    .await?
    {
        ForwardedChatResponse::Success(value) => value,
        ForwardedChatResponse::HttpError { status, body } => {
            return Ok(http_utils::http_json_response(status, &body));
        }
    };
    if requested_stream {
        let sse = convert_openai_chat_response_to_sse(&chat_response)?;
        Ok(http_utils::http_response(200, "text/event-stream", &sse))
    } else {
        Ok(http_utils::http_json_response(
            200,
            &serde_json::to_string(&chat_response)?,
        ))
    }
}

// =============================================================================
// PASSTHROUGH
// =============================================================================

/// Forwards a request as-is to the target provider (for non-API paths)
async fn forward_request(
    path: &str,
    request: &str,
    config: &Arc<ResponsesToChatRouterConfig>,
    client: &reqwest::Client,
) -> Result<String> {
    let body_str = http_utils::extract_request_body(request)?;

    let target_url = build_target_url(&config.target_base_url, path);

    let mut req = http_utils::authorized_openai_post(
        client,
        &target_url,
        &config.api_key,
        config.copilot_token_manager.as_deref(),
        None,
    )
    .await?;

    if !body_str.is_empty() {
        req = req.body(body_str.to_string());
    }

    let response = device_fingerprint::maybe_with_starter_headers(req, config.is_starter)
        .send_logged()
        .await?;
    http_utils::buffered_reqwest_to_http_response(response).await
}

async fn forward_openai_chat_request(
    body: &Value,
    config: &Arc<ResponsesToChatRouterConfig>,
    client: &reqwest::Client,
    force_non_streaming: bool,
    slot: &RouteSlot,
    learned_requires_reasoning: &Arc<AtomicBool>,
) -> Result<ForwardedChatResponse> {
    let active_protocol = slot.route_atom();
    // Openai and ResponsesApi both route through `forward_openai_protocol`,
    // so one is a byte-identical duplicate of the other. Drop the non-active.
    let active_proto = decode_route(active_protocol.load(Ordering::Relaxed)).0;
    let candidates: Vec<(ProviderProtocol, PathVariant)> = protocol_candidates(active_protocol)
        .into_iter()
        .filter(|(proto, _)| {
            !matches!(
                (active_proto, *proto),
                (ProviderProtocol::Openai, ProviderProtocol::ResponsesApi)
                    | (ProviderProtocol::ResponsesApi, ProviderProtocol::Openai)
            )
        })
        .collect();
    let mut first_error: Option<(u16, String)> = None;
    let mut body_for_attempts = body.clone();
    let mut retried_with_strict = false;
    let mut idx = 0;
    // Snapshot once so the retry decision matches what's actually on the wire:
    // when learned was already true at the start of this request, the body is
    // already strict and a retry would be a wasted round-trip.
    let effective_requires_reasoning =
        config.requires_reasoning_content || learned_requires_reasoning.load(Ordering::Relaxed);

    while idx < candidates.len() {
        let (protocol, variant) = candidates[idx];
        let attempt = idx;
        match forward_chat_for_protocol(
            protocol,
            variant,
            &body_for_attempts,
            config.as_ref(),
            client,
            force_non_streaming,
        )
        .await?
        {
            AttemptOutcome::Success(value) => {
                commit_protocol_switch(active_protocol, protocol, variant, attempt);
                slot.confirm();
                return Ok(ForwardedChatResponse::Success(value));
            }
            AttemptOutcome::Mismatch {
                status,
                body: response_body,
            } => {
                // A 4xx whose body is a recognizable LLM-API error envelope is
                // a semantic rejection — bail out instead of probing other
                // protocols that will return the same rejection.
                let classification = classify_failed_attempt(status, &response_body);
                if classification.is_terminal
                    || classification.is_semantic_rejection
                    || first_error.is_none()
                {
                    first_error = Some((status, response_body));
                }
                if classification.is_semantic_rejection {
                    slot.confirm();
                    if classification.quirk_hint == Some("requires_reasoning_content") {
                        learned_requires_reasoning.store(true, Ordering::Relaxed);
                        if !retried_with_strict && !effective_requires_reasoning {
                            retried_with_strict = true;
                            body_for_attempts = body.clone();
                            ensure_assistant_reasoning_content_in_chat_request(
                                &mut body_for_attempts,
                            );
                            continue;
                        }
                    }
                    if attempt > 0 {
                        commit_protocol_switch(active_protocol, protocol, variant, attempt);
                    }
                    break;
                }
                // Skip the fast-bail at attempt 0: a 401/403 there often means
                // "this host rejected the protocol's auth header shape" rather
                // than "your key is bad" (e.g. cross-protocol gateways). Probe
                // at least one fallback before believing the upstream.
                //
                // 429 doesn't get the carve-out: a rate-limit response is a
                // quota statement, not an auth-shape mismatch, and probing
                // more candidates against the same upstream piles requests
                // into the same already-overbudget window.
                if classification.is_terminal && (attempt > 0 || classification.is_rate_limited) {
                    // The path answered (with an error, but it answered) —
                    // pin it in memory so retry storms from codex/claude
                    // don't re-probe the wrong chat/completions paths every
                    // time. Don't flip request_succeeded: we still don't
                    // want to *persist* the pin to disk based on a 5xx.
                    commit_protocol_switch(active_protocol, protocol, variant, attempt);
                    break;
                }
            }
        }
        idx += 1;
    }

    let (status, body) = first_error.unwrap_or_default();
    Ok(ForwardedChatResponse::HttpError { status, body })
}

async fn forward_chat_for_protocol(
    protocol: ProviderProtocol,
    variant: PathVariant,
    body: &Value,
    config: &ResponsesToChatRouterConfig,
    client: &reqwest::Client,
    force_non_streaming: bool,
) -> Result<AttemptOutcome<Value>> {
    match protocol {
        ProviderProtocol::Openai | ProviderProtocol::ResponsesApi => {
            forward_openai_protocol(variant, body, config, client).await
        }
        ProviderProtocol::Anthropic => {
            forward_anthropic_protocol(variant, body, config, client, force_non_streaming).await
        }
        ProviderProtocol::Google => forward_google_protocol(body, config, client).await,
    }
}

async fn forward_openai_protocol(
    variant: PathVariant,
    body: &Value,
    config: &ResponsesToChatRouterConfig,
    client: &reqwest::Client,
) -> Result<AttemptOutcome<Value>> {
    let target_url = build_target_url(
        &config.target_base_url,
        variant.apply("/v1/chat/completions"),
    );
    let initiator = if config.copilot_token_manager.is_some() {
        Some(http_utils::copilot_initiator_from_openai(body))
    } else {
        None
    };
    let req = http_utils::authorized_openai_post(
        client,
        &target_url,
        &config.api_key,
        config.copilot_token_manager.as_deref(),
        initiator,
    )
    .await?;
    let response =
        device_fingerprint::maybe_with_starter_headers(req.json(body), config.is_starter)
            .send_logged()
            .await?;

    let status = response.status().as_u16();
    let body_text = response.text().await?;
    let parsed = if status == 200 {
        Some(parse_provider_response(&body_text)?)
    } else {
        None
    };
    let result = classify_attempt(status, body_text, parsed);

    // If Copilot rejected the model specifically because /chat/completions
    // is unsupported for it, fall back to /responses.
    if config.copilot_token_manager.is_some()
        && let AttemptOutcome::Mismatch {
            body: ref error_body,
            ..
        } = result
        && (error_body.contains("unsupported_api_for_model")
            || (error_body.contains("not support") && error_body.contains("chat/completions")))
        && let Ok(fallback) = try_copilot_responses_fallback(variant, body, config, client).await
    {
        return Ok(fallback);
    }

    Ok(result)
}

/// Converts a Chat Completions request to Responses API format and sends it to
/// Copilot's /responses endpoint. Returns the response converted back to Chat
/// Completions format.
async fn try_copilot_responses_fallback(
    variant: PathVariant,
    body: &Value,
    config: &ResponsesToChatRouterConfig,
    client: &reqwest::Client,
) -> Result<AttemptOutcome<Value>> {
    let responses_body = responses_chat_conversion::convert_chat_to_responses_request(body);
    let target_url = build_target_url(&config.target_base_url, variant.apply("/v1/responses"));
    let req = http_utils::authorized_openai_post(
        client,
        &target_url,
        &config.api_key,
        config.copilot_token_manager.as_deref(),
        None,
    )
    .await?;
    let response = device_fingerprint::maybe_with_starter_headers(
        req.json(&responses_body),
        config.is_starter,
    )
    .send_logged()
    .await?;

    let status = response.status().as_u16();
    let body_text = response.text().await?;
    if status == 200 {
        let resp_value: Value = serde_json::from_str(&body_text)?;
        let chat_value = responses_chat_conversion::convert_responses_json_to_chat(&resp_value);
        Ok(AttemptOutcome::Success(chat_value))
    } else {
        Ok(AttemptOutcome::Mismatch {
            status,
            body: body_text,
        })
    }
}

async fn forward_anthropic_protocol(
    variant: PathVariant,
    body: &Value,
    config: &ResponsesToChatRouterConfig,
    client: &reqwest::Client,
    force_non_streaming: bool,
) -> Result<AttemptOutcome<Value>> {
    let mut body_with_cache = body.clone();
    // Only inject cache_control for Claude models — other providers don't
    // honor it (e.g. Gemini uses a different caching model) and strict ones
    // reject the unknown field outright.
    if body_with_cache
        .get("model")
        .and_then(|m| m.as_str())
        .is_some_and(|m| m.to_ascii_lowercase().contains("claude"))
    {
        inject_chat_completions_cache_control(&mut body_with_cache);
    }

    let mut anthropic_body = convert_openai_chat_to_anthropic_request(
        &body_with_cache,
        &OpenAIToAnthropicChatConfig {
            default_model: "claude-sonnet-4-5",
        },
    );
    if force_non_streaming {
        anthropic_body["stream"] = json!(false);
    }

    let target_url = build_target_url(&config.target_base_url, variant.apply("/v1/messages"));
    let response = device_fingerprint::maybe_with_starter_headers(
        with_anthropic_messages_headers(client.post(&target_url), &config.api_key)
            .json(&anthropic_body),
        config.is_starter,
    )
    .send_logged()
    .await?;

    let status_code = response.status().as_u16();
    let response_text = response.text().await?;
    if status_code == 200 {
        let anthropic_response: Value = serde_json::from_str(&response_text)?;
        let openai_response = convert_anthropic_to_openai_chat_response(
            &anthropic_response,
            body.get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("gpt-4o"),
        );
        return Ok(AttemptOutcome::Success(openai_response));
    }
    // Anthropic error bodies wrap the OpenAI-shape `{"error":{...}}` in an
    // outer `{"type":"error", "error":{...}}` envelope. Codex/openai clients
    // can't parse that and fall back to generic "high demand" messages —
    // strip the wrap so the real upstream message reaches the user.
    let normalized = strip_anthropic_error_wrap(&response_text);
    Ok(classify_attempt::<Value>(status_code, normalized, None))
}

fn with_anthropic_messages_headers(
    builder: reqwest::RequestBuilder,
    api_key: &str,
) -> reqwest::RequestBuilder {
    builder
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("Content-Type", CONTENT_TYPE_JSON)
}

fn strip_anthropic_error_wrap(body: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    if parsed.get("type").and_then(|v| v.as_str()) == Some("error")
        && let Some(inner) = parsed.get("error")
    {
        return json!({ "error": inner }).to_string();
    }
    body.to_string()
}

async fn forward_google_protocol(
    body: &Value,
    config: &ResponsesToChatRouterConfig,
    client: &reqwest::Client,
) -> Result<AttemptOutcome<Value>> {
    let google_body = convert_openai_chat_to_gemini_request(
        body,
        &OpenAIToGeminiConfig {
            default_model: "gemini-2.5-pro",
        },
    );
    let model = openai_chat_model(body, "gemini-2.5-pro");
    let target_url = build_google_generate_content_url(&config.target_base_url, &model);
    let response = device_fingerprint::maybe_with_starter_headers(
        client
            .post(&target_url)
            .header("x-goog-api-key", config.api_key.as_str())
            .header("Content-Type", CONTENT_TYPE_JSON)
            .json(&google_body),
        config.is_starter,
    )
    .send_logged()
    .await?;

    let status_code = response.status().as_u16();
    let response_text = response.text().await?;
    let parsed = if status_code == 200 {
        let google_response: Value = serde_json::from_str(&response_text)?;
        Some(convert_gemini_to_openai_chat_response(
            &google_response,
            &model,
        ))
    } else {
        None
    };
    Ok(classify_attempt(status_code, response_text, parsed))
}

// =============================================================================
// URL HELPERS
// =============================================================================

/// Constructs target URL, avoiding /v1 duplication when base already ends with /v1
fn build_target_url(base_url: &str, path: &str) -> String {
    http_utils::build_target_url(base_url, path)
}

// =============================================================================
// TOOL FILTERING
// =============================================================================

/// Removes non-"function" tools from the request body.
/// If the tools array becomes empty, removes the key entirely.
fn filter_tools(body: &mut Value) {
    if let Some(tools) = body.get_mut("tools").and_then(|t| t.as_array_mut()) {
        tools.retain(|t| t.get("type").and_then(|v| v.as_str()) == Some("function"));
        if tools.is_empty() {
            body.as_object_mut().map(|o| o.remove("tools"));
        }
    }
}

// =============================================================================
// MODEL TRANSFORM
// =============================================================================

/// For OpenRouter, prefixes model with "openai/" if not already namespaced.
/// Also applies a custom prefix (e.g., "@cf/" for Cloudflare) if configured.
fn transform_model(body: &mut Value, base_url: &str, model_prefix: Option<&str>) {
    if let Some(model) = body["model"].as_str().map(String::from) {
        let transformed = transform_model_str(&model, base_url, model_prefix);
        if transformed != model {
            body["model"] = Value::String(transformed);
        }
    }
}

pub(crate) fn transform_model_str(
    model: &str,
    base_url: &str,
    model_prefix: Option<&str>,
) -> String {
    // First apply custom prefix (e.g., "@cf/" for Cloudflare)
    let with_prefix = if let Some(prefix) = model_prefix {
        if !model.starts_with(prefix) {
            format!("{}{}", prefix, model)
        } else {
            model.to_string()
        }
    } else {
        model.to_string()
    };

    // Then apply OpenRouter prefix if needed
    if is_openrouter_base(base_url) && !with_prefix.contains('/') {
        format!("openai/{}", with_prefix)
    } else {
        with_prefix
    }
}

fn apply_selected_model(
    body: &mut Value,
    config: &ResponsesToChatRouterConfig,
    protocol: ProviderProtocol,
) {
    if config.copilot_token_manager.is_some() {
        // For Copilot, only apply model name normalization (e.g. claude-sonnet-4-6 → claude-sonnet-4.6)
        if let Some(model) = body.get("model").and_then(|v| v.as_str()) {
            let copilot_name = crate::services::model_names::copilot_model_name(model);
            if copilot_name != model {
                body["model"] = Value::String(copilot_name);
            }
        }
        return;
    }

    let selected_model = select_model_for_provider_attempt(
        &config.target_base_url,
        body.get("model").and_then(|v| v.as_str()),
        config.actual_model.as_deref(),
        protocol,
    );
    let selected_model = if config
        .aivo_prefix_models
        .iter()
        .any(|m| m == &selected_model)
    {
        format!("aivo/{selected_model}")
    } else {
        selected_model
    };
    body["model"] = Value::String(selected_model);

    if protocol == ProviderProtocol::Openai {
        transform_model(
            body,
            &config.target_base_url,
            config.model_prefix.as_deref(),
        );
    }
}

// Conversion logic (Responses API ↔ Chat Completions) has been extracted to
// responses_chat_conversion.rs and is re-exported at the top of this file.

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn response_is_2xx_recognises_status_line() {
        assert!(response_is_2xx("HTTP/1.1 200 OK\r\n\r\n{}"));
        assert!(response_is_2xx("HTTP/1.1 204 No Content\r\n"));
        assert!(!response_is_2xx("HTTP/1.1 502 Bad Gateway\r\n"));
        assert!(!response_is_2xx("HTTP/1.1 429 Too Many Requests\r\n"));
        assert!(!response_is_2xx("garbage"));
    }

    fn mock_response(status: u16, body: &str) -> reqwest::Response {
        http::Response::builder()
            .status(status)
            .header("content-type", CONTENT_TYPE_JSON)
            .body(body.to_string())
            .unwrap()
            .into()
    }

    #[tokio::test]
    async fn note_streaming_failure_learns_reasoning_quirk_from_400() {
        // The exact DeepSeek thinking-mode rejection from the bug report: the
        // streaming bail must flip the learned flag so the buffered fallback's
        // first attempt is already strict instead of eating a second 400.
        let body = r#"{"error":{"message":"The reasoning_content in the thinking mode must be passed back to the API.","type":"invalid_request_error","code":"invalid_request_error"}}"#;
        let learned = AtomicBool::new(false);
        note_streaming_failure(mock_response(400, body), &learned).await;
        assert!(learned.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn note_streaming_failure_ignores_unrelated_400() {
        let body =
            r#"{"error":{"message":"context length exceeded","type":"invalid_request_error"}}"#;
        let learned = AtomicBool::new(false);
        note_streaming_failure(mock_response(400, body), &learned).await;
        assert!(!learned.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn note_streaming_failure_does_not_mislearn_when_field_is_rejected() {
        // Provider says it does NOT accept the field — flipping the flag would
        // inject reasoning_content into an upstream that rejects it.
        let body = r#"{"error":{"message":"Unknown field reasoning_content","type":"invalid_request_error"}}"#;
        let learned = AtomicBool::new(false);
        note_streaming_failure(mock_response(400, body), &learned).await;
        assert!(!learned.load(Ordering::Relaxed));
    }

    #[test]
    fn strip_anthropic_error_wrap_unwraps_minimax_500_body() {
        // Real body observed from minimax /v1/messages on a plan/billing error.
        let body = r#"{"type":"error","error":{"type":"api_error","message":"your current token plan not support model, MiniMax-M2.7 (2061)"},"request_id":"abc"}"#;
        let stripped = strip_anthropic_error_wrap(body);
        let parsed: Value = serde_json::from_str(&stripped).unwrap();
        // Outer envelope is gone; codex-readable shape: { error: { type, message } }.
        assert!(parsed.get("type").is_none());
        assert_eq!(
            parsed["error"]["message"],
            "your current token plan not support model, MiniMax-M2.7 (2061)"
        );
        assert_eq!(parsed["error"]["type"], "api_error");
    }

    #[test]
    fn strip_anthropic_error_wrap_passes_through_openai_shape() {
        let body = r#"{"error":{"message":"bad key","type":"invalid_request"}}"#;
        // No outer `"type":"error"` wrap → returned unchanged.
        assert_eq!(strip_anthropic_error_wrap(body), body);
    }

    #[test]
    fn anthropic_messages_headers_use_x_api_key_without_authorization() {
        let request = with_anthropic_messages_headers(
            reqwest::Client::new().post("http://127.0.0.1/v1/messages"),
            "sk-test",
        )
        .build()
        .unwrap();
        let headers = request.headers();

        assert_eq!(headers.get("x-api-key").unwrap(), "sk-test");
        assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
        assert_eq!(headers.get("content-type").unwrap(), CONTENT_TYPE_JSON);
        assert!(headers.get(reqwest::header::AUTHORIZATION).is_none());
    }

    #[test]
    fn strip_anthropic_error_wrap_passes_through_invalid_json() {
        let body = "<html>504 Gateway Timeout</html>";
        assert_eq!(strip_anthropic_error_wrap(body), body);
    }

    #[test]
    fn prepare_chat_completions_body_adds_reasoning_for_plain_assistant_in_strict_mode() {
        let config = ResponsesToChatRouterConfig {
            target_base_url: "https://api.example.com".to_string(),
            api_key: "sk-test".to_string(),
            target_protocol: ProviderProtocol::Openai,
            target_path_variant: None,
            copilot_token_manager: None,
            model_prefix: None,
            requires_reasoning_content: true,
            actual_model: None,
            max_tokens_cap: None,
            responses_api_supported: None,
            is_starter: false,
            aivo_prefix_models: Vec::new(),
        };
        let body = json!({
            "model": "gpt-4o",
            "messages": [{"role": "assistant", "content": "OK, continuing."}]
        });

        let prepared =
            prepare_chat_completions_body(&body, &config, ProviderProtocol::Openai, true);
        let messages = prepared["messages"].as_array().unwrap();
        assert_eq!(messages[0]["reasoning_content"], "OK, continuing.");
    }

    #[test]
    fn prepare_chat_completions_body_obeys_bool_param_not_config_field() {
        // The bool param is what callers OR with the runtime-learned flag — if
        // we still read `config.requires_reasoning_content` here, every request
        // in the same launch would pay one wasted 400 + retry round-trip until
        // process exit, and only the persisted-to-keystore quirk would help on
        // the *next* launch.
        let config = ResponsesToChatRouterConfig {
            target_base_url: "https://api.example.com".to_string(),
            api_key: "sk-test".to_string(),
            target_protocol: ProviderProtocol::Openai,
            target_path_variant: None,
            copilot_token_manager: None,
            model_prefix: None,
            requires_reasoning_content: false, // config says no
            actual_model: None,
            max_tokens_cap: None,
            responses_api_supported: None,
            is_starter: false,
            aivo_prefix_models: Vec::new(),
        };
        let body = json!({
            "model": "gpt-4o",
            "messages": [{"role": "assistant", "content": "OK, continuing."}]
        });

        // Caller passes `true` (e.g. learned arc said so) — body must be strict.
        let prepared =
            prepare_chat_completions_body(&body, &config, ProviderProtocol::Openai, true);
        let messages = prepared["messages"].as_array().unwrap();
        assert_eq!(messages[0]["reasoning_content"], "OK, continuing.");

        // Caller passes `false` — body must NOT carry reasoning_content even
        // if some future config field is added that we'd otherwise read.
        let prepared =
            prepare_chat_completions_body(&body, &config, ProviderProtocol::Openai, false);
        let messages = prepared["messages"].as_array().unwrap();
        assert!(messages[0].get("reasoning_content").is_none());
    }

    // ── HTTP body extraction ────────────────────────────────────────────────────

    #[test]
    fn test_extract_request_body_normal() {
        let req = "POST /v1/chat/completions HTTP/1.1\r\nContent-Type: application/json\r\n\r\n{\"model\":\"gpt-4\"}";
        assert_eq!(
            http_utils::extract_request_body(req).unwrap(),
            "{\"model\":\"gpt-4\"}"
        );
    }

    #[test]
    fn test_extract_request_body_missing_separator_returns_error() {
        let req = "POST /v1/chat/completions HTTP/1.1";
        assert!(http_utils::extract_request_body(req).is_err());
    }

    #[test]
    fn test_extract_request_body_short_request_no_panic() {
        assert!(http_utils::extract_request_body("AB").is_err());
    }

    // ── Tool filtering ─────────────────────────────────────────────────────────

    #[test]
    fn test_filter_tools_removes_non_function() {
        let mut body = json!({
            "model": "gpt-4",
            "tools": [
                {"type": "function", "function": {"name": "my_fn"}},
                {"type": "computer_use"},
                {"type": "file_search"},
                {"type": "web_search"},
                {"type": "code_interpreter"}
            ]
        });

        filter_tools(&mut body);

        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
    }

    #[test]
    fn test_filter_tools_all_non_function_removes_key() {
        let mut body = json!({
            "model": "gpt-4",
            "tools": [{"type": "computer_use"}, {"type": "web_search"}]
        });
        filter_tools(&mut body);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn test_filter_tools_already_function_only_unchanged() {
        let mut body = json!({
            "model": "gpt-4",
            "tools": [
                {"type": "function", "function": {"name": "fn1"}},
                {"type": "function", "function": {"name": "fn2"}}
            ]
        });
        filter_tools(&mut body);
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_filter_tools_no_tools_key_is_noop() {
        let mut body = json!({"model": "gpt-4", "messages": []});
        filter_tools(&mut body);
        assert!(body.get("tools").is_none());
        assert_eq!(body["model"], "gpt-4");
    }

    // ── Model transform ────────────────────────────────────────────────────────

    #[test]
    fn test_transform_model_openrouter_adds_prefix() {
        let mut body = json!({"model": "gpt-4o"});
        transform_model(&mut body, "https://openrouter.ai/api/v1", None);
        assert_eq!(body["model"], "openai/gpt-4o");
    }

    #[test]
    fn test_transform_model_openrouter_already_prefixed() {
        let mut body = json!({"model": "openai/gpt-4o"});
        transform_model(&mut body, "https://openrouter.ai/api/v1", None);
        assert_eq!(body["model"], "openai/gpt-4o");
    }

    #[test]
    fn test_transform_model_non_openrouter_passthrough() {
        let mut body = json!({"model": "gpt-4o"});
        transform_model(&mut body, "https://ai-gateway.vercel.sh/v1", None);
        assert_eq!(body["model"], "gpt-4o");
    }

    #[test]
    fn test_transform_model_cloudflare_prefix() {
        let mut body = json!({"model": "glm-4.7-flash"});
        transform_model(
            &mut body,
            "https://api.cloudflare.com/client/v4/accounts/abc/ai/v1",
            Some("@cf/"),
        );
        assert_eq!(body["model"], "@cf/glm-4.7-flash");
    }

    #[test]
    fn test_transform_model_cloudflare_prefix_already_present() {
        let mut body = json!({"model": "@cf/llama-3.1-8b"});
        transform_model(
            &mut body,
            "https://api.cloudflare.com/client/v4/accounts/abc/ai/v1",
            Some("@cf/"),
        );
        assert_eq!(body["model"], "@cf/llama-3.1-8b");
    }

    fn test_router_config(
        target: &str,
        aivo_prefix_models: Vec<String>,
    ) -> ResponsesToChatRouterConfig {
        ResponsesToChatRouterConfig {
            target_base_url: target.to_string(),
            api_key: "sk-test".to_string(),
            target_protocol: ProviderProtocol::Openai,
            target_path_variant: None,
            copilot_token_manager: None,
            model_prefix: None,
            requires_reasoning_content: false,
            actual_model: None,
            max_tokens_cap: None,
            responses_api_supported: None,
            is_starter: false,
            aivo_prefix_models,
        }
    }

    #[test]
    fn apply_selected_model_re_adds_aivo_prefix_when_listed() {
        // Regression: opencode's SDK strips `aivo/` from `aivo/starter` so
        // the body arrives as `starter`. Without re-prefix the upstream
        // returns "model not found: starter".
        let config = test_router_config("https://api.getaivo.dev", vec!["starter".to_string()]);
        let mut body = json!({"model": "starter"});
        apply_selected_model(&mut body, &config, ProviderProtocol::Openai);
        assert_eq!(body["model"], "aivo/starter");
    }

    #[test]
    fn apply_selected_model_passes_through_non_aivo_prefixed_models() {
        // Vendor-namespaced ids (e.g. `minimax/minimax-m2.7`) ride through
        // unchanged — only the bare names listed in `aivo_prefix_models`
        // get re-prefixed.
        let config = test_router_config("https://api.getaivo.dev", vec!["starter".to_string()]);
        let mut body = json!({"model": "minimax/minimax-m2.7"});
        apply_selected_model(&mut body, &config, ProviderProtocol::Openai);
        assert_eq!(body["model"], "minimax/minimax-m2.7");
    }

    #[test]
    fn apply_selected_model_noop_without_starter_catalog() {
        let config = test_router_config("https://api.example.com", vec![]);
        let mut body = json!({"model": "starter"});
        apply_selected_model(&mut body, &config, ProviderProtocol::Openai);
        assert_eq!(body["model"], "starter");
    }

    // ── URL building ───────────────────────────────────────────────────────────

    #[test]
    fn test_build_target_url_strips_v1_duplication() {
        let url = build_target_url("https://ai-gateway.vercel.sh/v1", "/v1/responses");
        assert_eq!(url, "https://ai-gateway.vercel.sh/v1/responses");
    }

    #[test]
    fn test_build_target_url_no_v1_in_path() {
        let url = build_target_url("https://ai-gateway.vercel.sh/v1", "/responses");
        assert_eq!(url, "https://ai-gateway.vercel.sh/v1/responses");
    }

    #[test]
    fn test_build_target_url_base_no_v1() {
        let url = build_target_url("https://api.example.com", "/v1/responses");
        assert_eq!(url, "https://api.example.com/v1/responses");
    }
}
