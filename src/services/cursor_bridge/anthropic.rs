//! Anthropic `/v1/messages` adapter for cursor-backed tools (claude).
//! Translates inbound messages requests into cursor-agent ACP prompts and
//! streams cursor's responses back as Anthropic SSE (`message_start`,
//! `content_block_*`, `message_stop`). Tool-using turns route through the
//! [`super::mcp`] bridge; the resumption path matches a `tool_result` to a
//! still-running ACP prompt and unblocks the parked MCP `tools/call`.

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::services::acp_client::PromptEvent;
use crate::services::cursor_acp::{self, CURSOR_ACP_SENTINEL, CursorAcpSession};
use crate::services::http_utils::{
    cors_header_block, extract_request_body, http_chunked_response_head_with_extra,
};

use super::mcp::{BridgeEvent, BridgeSession, McpBridge, ToolUseIdStyle};
use super::*;

// === Anthropic messages ===

pub(super) async fn handle_anthropic_messages(
    socket: &mut TcpStream,
    state: &RouterState,
    request: &str,
) -> (u16, Option<String>) {
    match run_anthropic_messages(socket, state, request).await {
        Ok(summary) => (200, summary),
        Err(err) => {
            let status = status_for_handler_error(&err);
            let msg = err.to_string();
            let _ = write_json_error(socket, status, &msg).await;
            (status, Some(msg))
        }
    }
}

pub(super) async fn run_anthropic_messages(
    socket: &mut TcpStream,
    state: &RouterState,
    request: &str,
) -> Result<Option<String>> {
    let body_str = extract_request_body(request).context("read request body")?;
    let body: Value =
        serde_json::from_str(body_str).context("parse Anthropic messages request body")?;
    let stream_flag = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);

    // Title-gen short-circuit. Claude Code fires this in parallel with every
    // real turn — forwarding to cursor costs 60-100 s of full-model time.
    // Skip the transcript reduction + image-block walk on this path; only
    // the first user message is needed to derive the title.
    if is_title_generation_request(&body) {
        let model = requested_model.unwrap_or_else(|| CURSOR_ACP_SENTINEL.to_string());
        let user_text = extract_first_user_text(&body).unwrap_or_default();
        let title = build_title_from_user_text(&user_text);
        return short_circuit_title_response(
            socket,
            &model,
            &title,
            stream_flag,
            estimate_tokens(&user_text),
        )
        .await;
    }

    // Non-streaming resumption: a `tool_result` for a previously-parked
    // call must still be drained or cursor-agent's session sits idle for
    // up to TOOL_CALL_PARK_TIMEOUT (10 min). Deliver the result, tear
    // the bridge session down, and let the legacy text-flatten path
    // handle the actual response.
    if !stream_flag && let Some((tool_use_id, content, is_error)) = extract_last_tool_result(&body)
    {
        state
            .mcp_bridge
            .deliver_and_drop_parked(&tool_use_id, content, is_error)
            .await;
    }

    // Streaming + tools array → bridge path: tools are exposed to the
    // cursor model via an in-process MCP server, and tool_use blocks flow
    // back as Anthropic SSE content blocks instead of being flattened to
    // text. Resumption turns (whose last user message contains a
    // tool_result) re-attach to the still-running ACP prompt.
    if stream_flag && anthropic_request_uses_tools(&body) {
        return run_anthropic_bridged(socket, state, body, requested_model).await;
    }

    let parsed = ParsedTurn {
        stream_flag,
        requested_model,
        image_blocks: extract_anthropic_image_blocks(&body)?,
        prompt: reduce_anthropic_request_to_prompt(&body),
    };
    if parsed.prompt.trim().is_empty() && parsed.image_blocks.is_empty() {
        return Err(anyhow!("reduced prompt is empty; no user-visible message"));
    }

    run_turn(
        socket,
        state,
        parsed,
        CURSOR_ACP_SENTINEL,
        stream_anthropic_sse,
        anthropic_message_body,
    )
    .await
}

// === Anthropic /v1/messages with MCP-bridged client tools ===
//
// Path is taken whenever the inbound body declares `tools: [...]` and
// `stream: true`. Tools flow to the cursor model via the [`McpBridge`] HTTP
// server registered in `session/new`'s `mcpServers`; tool calls come back
// out of cursor as MCP `tools/call` POSTs which we translate into
// Anthropic `tool_use` content blocks on the SSE stream.

pub(super) fn anthropic_request_uses_tools(body: &Value) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty())
}

/// Pulls `tools: [...]` from the inbound request body. The schemas are
/// passed through unchanged to the MCP server, which performs the
/// `input_schema` → `inputSchema` rename when cursor-agent calls
/// `tools/list`.
pub(super) fn extract_anthropic_tools(body: &Value) -> Vec<Value> {
    body.get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// On the Anthropic resumption path, the cursor session is mid-prompt
/// waiting on its MCP `tools/call`. Only the tool_result can be delivered
/// back through MCP — any sibling content blocks (text, image, document)
/// in the same user message would be silently dropped. Emit a one-shot
/// stderr warning so the silent-loss case at least surfaces in logs.
pub(super) fn warn_if_resumption_drops_blocks(body: &Value) {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return;
    };
    let Some(last) = messages.last() else {
        return;
    };
    if last.get("role").and_then(Value::as_str) != Some("user") {
        return;
    }
    let Some(blocks) = last.get("content").and_then(Value::as_array) else {
        return;
    };
    let mut has_tool_result = false;
    let mut dropped_kinds: Vec<&str> = Vec::new();
    for block in blocks {
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "tool_result" {
            has_tool_result = true;
        } else if !kind.is_empty() && !dropped_kinds.contains(&kind) {
            dropped_kinds.push(kind);
        }
    }
    if has_tool_result && !dropped_kinds.is_empty() {
        eprintln!(
            "aivo: cursor bridge resumption is dropping non-tool_result blocks in the user \
             message: {dropped_kinds:?}. The cursor model only sees the tool_result; other \
             content is lost. Send a fresh /v1/messages turn to deliver additional content."
        );
    }
}

/// Find the first `tool_result` block in the final user message and return
/// (`tool_use_id`, MCP-shaped content array, `is_error`). Returns `None`
/// when the last message isn't a user message or carries no `tool_result`.
/// Multiple `tool_result` blocks in one user message (parallel tool_use)
/// are not yet supported — we take the first.
pub(super) fn extract_last_tool_result(body: &Value) -> Option<(String, Vec<Value>, bool)> {
    let messages = body.get("messages")?.as_array()?;
    let last = messages.last()?;
    if last.get("role").and_then(Value::as_str)? != "user" {
        return None;
    }
    let blocks = last.get("content")?.as_array()?;
    for block in blocks {
        if block.get("type").and_then(Value::as_str)? != "tool_result" {
            continue;
        }
        let id = block
            .get("tool_use_id")
            .and_then(Value::as_str)?
            .to_string();
        let is_error = block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let content = match block.get("content") {
            Some(Value::String(s)) => vec![json!({"type": "text", "text": s})],
            Some(Value::Array(arr)) => arr.clone(),
            _ => Vec::new(),
        };
        return Some((id, content, is_error));
    }
    None
}

pub(super) async fn run_anthropic_bridged(
    socket: &mut TcpStream,
    state: &RouterState,
    body: Value,
    requested_model: Option<String>,
) -> Result<Option<String>> {
    // Resumption — claude-cli is delivering a tool_result for a previously
    // parked tool_use. Try to match it to a still-running ACP session.
    if let Some((tool_use_id, content, is_error)) = extract_last_tool_result(&body)
        && let Some(session) = state
            .mcp_bridge
            .resume_with_tool_result(&tool_use_id, content, is_error)
            .await
    {
        warn_if_resumption_drops_blocks(&body);
        return run_anthropic_bridged_resume(socket, state, session, &body, requested_model).await;
    }

    // Fresh path — no matching parked call (or first turn). Open a new
    // bridge session + cursor ACP session pinned to it.
    run_anthropic_bridged_fresh(socket, state, body, requested_model).await
}

pub(super) async fn run_anthropic_bridged_fresh(
    socket: &mut TcpStream,
    state: &RouterState,
    body: Value,
    requested_model: Option<String>,
) -> Result<Option<String>> {
    let tools = extract_anthropic_tools(&body);
    let image_blocks = extract_anthropic_image_blocks(&body)?;
    let prompt = reduce_anthropic_request_to_prompt_without_tools(&body);
    if prompt.trim().is_empty() && image_blocks.is_empty() {
        return Err(anyhow!("reduced prompt is empty; no user-visible message"));
    }
    let input_tokens = estimate_tokens(&prompt);

    let (bridge_session, mut acp) = if let Some(slot) = take_mcp_prewarmed(state).await {
        McpBridge::take_for_use(&slot.bridge_session, tools).await;
        (slot.bridge_session, slot.acp)
    } else {
        let (bridge_session, mcp_url) = state
            .mcp_bridge
            .open_session(tools, ToolUseIdStyle::Anthropic)
            .await;
        let bridge_id = { bridge_session.lock().await.id.clone() };

        let acp_result = CursorAcpSession::open_with_mcp(
            &state.config.key,
            requested_model.as_deref(),
            &state.config.workspace_cwd,
            Some(&mcp_url),
        )
        .await
        .context("open cursor-agent ACP session with MCP bridge");

        match acp_result {
            Ok(s) => (bridge_session, s),
            Err(e) => {
                state.mcp_bridge.drop_session(&bridge_id).await;
                return Err(e);
            }
        }
    };
    let bridge_id = { bridge_session.lock().await.id.clone() };

    if let Some(model) = &requested_model
        && let Err(e) = acp.set_model(model).await
    {
        state.mcp_bridge.drop_session(&bridge_id).await;
        return Err(e).context("cursor-agent set_model");
    }
    if !image_blocks.is_empty() && !acp.supports_image_prompts() {
        state.mcp_bridge.drop_session(&bridge_id).await;
        return Err(anyhow!(image_capability_error()));
    }

    let response_model = acp
        .model_id()
        .map(str::to_string)
        .or(requested_model.clone())
        .unwrap_or_else(|| CURSOR_ACP_SENTINEL.to_string());

    let blocks = cursor_acp::assemble_prompt_blocks(&prompt, image_blocks);
    let stream = match acp.prompt_with_blocks(blocks).await {
        Ok(s) => s,
        Err(e) => {
            state.mcp_bridge.drop_session(&bridge_id).await;
            return Err(e).context("cursor-agent session/prompt");
        }
    };

    {
        let mut guard = bridge_session.lock().await;
        guard.attach_session(acp, stream);
    }

    stream_bridged_turn(
        socket,
        state,
        bridge_session,
        &bridge_id,
        &response_model,
        input_tokens,
    )
    .await
}

pub(super) async fn run_anthropic_bridged_resume(
    socket: &mut TcpStream,
    state: &RouterState,
    bridge_session: Arc<tokio::sync::Mutex<BridgeSession>>,
    body: &Value,
    requested_model: Option<String>,
) -> Result<Option<String>> {
    let bridge_id = { bridge_session.lock().await.id.clone() };
    // Use the same metric as the OpenAI/Responses/Gemini resume paths so
    // usage stats aggregated across protocols stay comparable per turn.
    let input_tokens = estimate_tokens(&reduce_anthropic_request_to_prompt_without_tools(body));
    let response_model = requested_model.unwrap_or_else(|| CURSOR_ACP_SENTINEL.to_string());

    stream_bridged_turn(
        socket,
        state,
        bridge_session,
        &bridge_id,
        &response_model,
        input_tokens,
    )
    .await
}

/// SSE loop that multiplexes ACP `session/update` events and MCP-bridge
/// `tool_call` events onto the same Anthropic-format response. Exits on
/// either `end_turn` (cleanup + drop bridge session) or `tool_use`
/// (preserve bridge session so the next `/v1/messages` can resume it).
pub(super) async fn stream_bridged_turn(
    socket: &mut TcpStream,
    state: &RouterState,
    bridge_session: Arc<tokio::sync::Mutex<BridgeSession>>,
    bridge_id: &str,
    response_model: &str,
    input_tokens: u64,
) -> Result<Option<String>> {
    let (acp, mut stream, mut event_rx) = match async {
        let mut guard = bridge_session.lock().await;
        let (acp, stream) = guard.take_active()?;
        let rx = guard.attach_event_sink();
        Ok::<_, anyhow::Error>((acp, stream, rx))
    }
    .await
    {
        Ok(triple) => triple,
        Err(e) => {
            // Race: bridge session is in the sessions map but its ACP
            // session / prompt stream was already taken (or never attached).
            // Tear it down and surface as a 500 instead of panicking.
            state.mcp_bridge.drop_session(bridge_id).await;
            return Err(e).context("bridge session lost its active ACP slot");
        }
    };

    let head = http_chunked_response_head_with_extra(200, "text/event-stream", cors_header_block());
    if let Err(e) = socket.write_all(head.as_bytes()).await {
        // Client closed on us before we wrote a byte; tear the bridge down
        // wholesale — there's no resumption that could save this turn.
        {
            let mut guard = bridge_session.lock().await;
            guard.detach_event_sink();
        }
        drop(acp);
        drop(stream);
        state.mcp_bridge.drop_session(bridge_id).await;
        return Err(e).context("write SSE head");
    }

    let message_id = new_anthropic_message_id();
    let _ = write_sse_chunk(
        socket,
        &sse_named_event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": message_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": response_model,
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": 0,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0,
                    },
                },
            }),
        ),
    )
    .await;

    let mut block_state = AnthropicBlockState::default();
    let mut stop_reason = "end_turn";
    let mut output_tokens: u64 = 0;
    let mut aggregated = String::new();
    let mut parked = false;
    let mut turn_errored = false;

    'outer: loop {
        tokio::select! {
            biased;
            // Drain MCP-bridge events first so a tool_call mid-stream
            // doesn't get reordered behind a same-tick text chunk.
            ev = event_rx.recv() => {
                match ev {
                    Some(BridgeEvent::ToolCall { tool_use_id, name, arguments }) => {
                        // Close any open text/thinking block before opening
                        // the tool_use one — Anthropic blocks don't nest.
                        let _ = block_state.close(socket).await;
                        let idx = block_state.allocate_index();
                        let _ = write_sse_chunk(
                            socket,
                            &anthropic_content_block_start_tool_use(idx, &tool_use_id, &name),
                        )
                        .await;
                        let _ = write_sse_chunk(
                            socket,
                            &anthropic_input_json_delta(idx, &arguments),
                        )
                        .await;
                        let _ = write_sse_chunk(socket, &anthropic_content_block_stop(idx)).await;
                        stop_reason = "tool_use";
                        parked = true;
                        break 'outer;
                    }
                    None => {
                        // Bridge session was torn down — finish gracefully.
                        break 'outer;
                    }
                }
            }
            ev = stream.next() => {
                match ev {
                    Some(PromptEvent::Update(value)) => {
                        if let Some(text) = extract_agent_text(&value) {
                            aggregated.push_str(text);
                            output_tokens = output_tokens.saturating_add(estimate_tokens(text));
                            let _ = block_state
                                .ensure_kind(socket, AnthropicBlockKind::Text)
                                .await;
                            let _ = write_sse_chunk(
                                socket,
                                &anthropic_text_delta(block_state.index(), text),
                            )
                            .await;
                        } else if let Some(thought) = extract_agent_thought(&value) {
                            let _ = block_state
                                .ensure_kind(socket, AnthropicBlockKind::Thinking)
                                .await;
                            let _ = write_sse_chunk(
                                socket,
                                &anthropic_thinking_delta(block_state.index(), thought),
                            )
                            .await;
                        } else if let Some(marker) = extract_tool_call_marker(&value) {
                            let _ = block_state
                                .ensure_kind(socket, AnthropicBlockKind::Thinking)
                                .await;
                            let _ = write_sse_chunk(
                                socket,
                                &anthropic_thinking_delta(block_state.index(), &marker),
                            )
                            .await;
                        }
                    }
                    Some(PromptEvent::Done(result)) => {
                        if result.is_err() {
                            // Anthropic's stop_reason enum is closed-set;
                            // emit a spec-valid `end_turn` and surface the
                            // upstream error via cancellation+logging.
                            turn_errored = true;
                        }
                        break 'outer;
                    }
                    None => break 'outer,
                }
            }
        }
    }

    if turn_errored && !parked {
        // Tell cursor-agent's session/prompt to stop so its child doesn't
        // keep generating output we'll never deliver. Best-effort: a dead
        // session simply errors here and is dropped below.
        let _ = acp.cancel().await;
    }

    let _ = block_state.close(socket).await;
    let _ = write_sse_chunk(
        socket,
        &sse_named_event(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                },
            }),
        ),
    )
    .await;
    let _ = write_sse_chunk(
        socket,
        &sse_named_event("message_stop", &json!({"type": "message_stop"})),
    )
    .await;
    let _ = write_chunk_terminator(socket).await;

    {
        let mut guard = bridge_session.lock().await;
        guard.detach_event_sink();
        if parked {
            // Preserve the ACP session for the resumption turn that will
            // arrive on a follow-up `/v1/messages` carrying the matching
            // `tool_result`.
            guard.return_active(acp, stream);
        } else {
            // Drop the ACP session; the bridge session map entry is cleaned
            // up below. Holding `acp`/`stream` past this scope would just
            // delay the underlying child-process shutdown.
            drop(acp);
            drop(stream);
        }
    }
    if !parked {
        state.mcp_bridge.drop_session(bridge_id).await;
    }

    Ok(if aggregated.is_empty() {
        None
    } else {
        Some(aggregated)
    })
}

/// Open a fresh content_block_start for an Anthropic `tool_use` block.
/// The `input` field must start empty — the actual JSON arguments arrive
/// as an `input_json_delta` so the client streams the schema with the
/// same incremental shape it expects from real upstream tool calls.
pub(super) fn anthropic_content_block_start_tool_use(index: u32, id: &str, name: &str) -> String {
    sse_named_event(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": {},
            },
        }),
    )
}

/// Emit the full tool_use arguments as a single `input_json_delta`. We
/// don't have to stream partial JSON because the MCP `tools/call` body
/// arrived complete from cursor-agent before we entered the SSE loop.
pub(super) fn anthropic_input_json_delta(index: u32, arguments: &Value) -> String {
    sse_named_event(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {
                "type": "input_json_delta",
                "partial_json": arguments.to_string(),
            },
        }),
    )
}

/// Variant of [`reduce_anthropic_request_to_prompt`] used by the bridged
/// path: skips the `Available tools:` text header because tools now flow
/// to the cursor model through the in-process MCP server instead. The
/// `tool_use` / `tool_result` transcript markers are still kept so the
/// model can see prior tool loops in the conversation history.
pub(super) fn reduce_anthropic_request_to_prompt_without_tools(body: &Value) -> String {
    let mut parts = Vec::new();
    let system_text = extract_anthropic_system_text(body.get("system"));
    if !system_text.trim().is_empty() {
        parts.push(format!("System: {system_text}"));
    }
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return parts.join("\n\n");
    };
    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
        let label = match role {
            "user" => "User",
            "assistant" => "Assistant",
            other => other,
        };
        for entry in flatten_anthropic_message_blocks(label, msg.get("content")) {
            parts.push(entry);
        }
    }
    parts.join("\n\n")
}

/// Detects Claude Code's title-generation subagent request by its
/// system-prompt signature. Matching three distinct fragments keeps false
/// positives very unlikely — a coding request would have to coincidentally
/// contain all three to be misclassified.
pub(super) fn is_title_generation_request(body: &Value) -> bool {
    let system_text = extract_anthropic_system_text(body.get("system"));
    system_text.contains("Generate a concise")
        && system_text.contains("sentence-case title")
        && system_text.contains("Return JSON")
}

/// Pulls a reasonable conversation title out of the user-visible messages.
/// Falls back to a static label only when the body carries no usable text.
#[cfg(test)]
pub(super) fn build_title_from_anthropic_body(body: &Value) -> String {
    build_title_from_user_text(&extract_first_user_text(body).unwrap_or_default())
}

pub(super) fn build_title_from_user_text(user_text: &str) -> String {
    if user_text.trim().is_empty() {
        "Coding session".to_string()
    } else {
        compose_short_title(user_text)
    }
}

pub(super) fn extract_first_user_text(body: &Value) -> Option<String> {
    let messages = body.get("messages").and_then(Value::as_array)?;
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) == Some("user") {
            let text = collect_anthropic_text(msg.get("content"));
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
    }
    None
}

pub(super) fn collect_anthropic_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut acc = String::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(t) = block.get("text").and_then(Value::as_str)
                {
                    if !acc.is_empty() {
                        acc.push(' ');
                    }
                    acc.push_str(t);
                }
            }
            acc
        }
        _ => String::new(),
    }
}

/// Truncates a free-form prompt to a 3-7 word title, breaking on word
/// boundaries and capping at ~60 visible chars to match Claude Code's UI
/// expectations for the session label.
pub(super) fn compose_short_title(raw: &str) -> String {
    let trimmed = raw.trim();
    let first_line = trimmed.lines().next().unwrap_or("").trim();
    let words: Vec<&str> = first_line.split_whitespace().take(7).collect();
    if words.is_empty() {
        return "Coding session".to_string();
    }
    let mut title = words.join(" ");
    if title.chars().count() > 60 {
        let truncated: String = title.chars().take(60).collect();
        let cut = truncated
            .rfind(' ')
            .map(|i| truncated[..i].to_string())
            .unwrap_or(truncated);
        title = cut;
    }
    title
}

/// Emits a hardcoded Anthropic response with a JSON `{"title":"..."}` body,
/// skipping any cursor work. Supports both streaming and one-shot modes so
/// Claude Code sees a normal `/v1/messages` reply.
pub(super) async fn short_circuit_title_response(
    socket: &mut TcpStream,
    model: &str,
    title: &str,
    stream_flag: bool,
    input_tokens: u64,
) -> Result<Option<String>> {
    let json_content = json!({"title": title}).to_string();
    if !stream_flag {
        let turn = AggregatedTurn {
            content: json_content.clone(),
            reasoning: String::new(),
        };
        let body = anthropic_message_body(&turn, model, input_tokens);
        write_json_response(socket, 200, &body).await?;
        return Ok(Some(json_content));
    }

    let head = http_chunked_response_head_with_extra(200, "text/event-stream", cors_header_block());
    socket.write_all(head.as_bytes()).await?;

    let message_id = new_anthropic_message_id();
    let output_tokens = estimate_tokens(&json_content);

    write_sse_chunk(
        socket,
        &sse_named_event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": message_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": model,
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": 0,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0,
                    },
                },
            }),
        ),
    )
    .await?;
    write_sse_chunk(
        socket,
        &anthropic_content_block_start(0, AnthropicBlockKind::Text),
    )
    .await?;
    write_sse_chunk(socket, &anthropic_text_delta(0, &json_content)).await?;
    write_sse_chunk(socket, &anthropic_content_block_stop(0)).await?;
    write_sse_chunk(
        socket,
        &sse_named_event(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                },
            }),
        ),
    )
    .await?;
    write_sse_chunk(
        socket,
        &sse_named_event("message_stop", &json!({"type": "message_stop"})),
    )
    .await?;
    write_chunk_terminator(socket).await?;
    Ok(Some(json_content))
}

pub(super) async fn stream_anthropic_sse(
    socket: &mut TcpStream,
    stream: &mut crate::services::acp_client::PromptStream,
    model: &str,
    input_tokens: u64,
) -> Result<String> {
    let head = http_chunked_response_head_with_extra(200, "text/event-stream", cors_header_block());
    socket.write_all(head.as_bytes()).await?;

    let message_id = new_anthropic_message_id();
    write_sse_chunk(
        socket,
        &sse_named_event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": message_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": model,
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": 0,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0,
                    },
                },
            }),
        ),
    )
    .await?;
    // Content blocks open lazily so Claude sees a clean interleaving of
    // `thinking` (cursor's reasoning + tool-call titles) and `text` (the
    // agent's user-visible message). Each transition closes the current
    // block and starts the next one at a fresh index — the protocol allows
    // multiple blocks per message and Claude Code's UI uses block type to
    // pick its renderer (collapsible "Cogitated…" panel vs. message bubble).
    let mut block_state = AnthropicBlockState::default();
    let mut stop_reason = "end_turn";
    let mut output_tokens: u64 = 0;
    let mut aggregated = String::new();
    while let Some(event) = stream.next().await {
        match event {
            PromptEvent::Update(value) => {
                if let Some(text) = extract_agent_text(&value) {
                    aggregated.push_str(text);
                    output_tokens = output_tokens.saturating_add(estimate_tokens(text));
                    block_state
                        .ensure_kind(socket, AnthropicBlockKind::Text)
                        .await?;
                    write_sse_chunk(socket, &anthropic_text_delta(block_state.index(), text))
                        .await?;
                } else if let Some(thought) = extract_agent_thought(&value) {
                    block_state
                        .ensure_kind(socket, AnthropicBlockKind::Thinking)
                        .await?;
                    write_sse_chunk(
                        socket,
                        &anthropic_thinking_delta(block_state.index(), thought),
                    )
                    .await?;
                } else if let Some(marker) = extract_tool_call_marker(&value) {
                    // Surface cursor's tool-call titles as inline thinking
                    // text. Claude Code shows them inside the "Cogitated…"
                    // panel — without this, the user sees no progress at all
                    // while cursor runs (or tries to run) tools, and the
                    // status indicator can stall for tens of seconds.
                    block_state
                        .ensure_kind(socket, AnthropicBlockKind::Thinking)
                        .await?;
                    write_sse_chunk(
                        socket,
                        &anthropic_thinking_delta(block_state.index(), &marker),
                    )
                    .await?;
                }
                // available_commands_update / session_info_update etc. are
                // pure protocol overhead and intentionally dropped.
            }
            PromptEvent::Done(result) => {
                if result.is_err() {
                    stop_reason = "error";
                }
                break;
            }
        }
    }

    block_state.close(socket).await?;
    write_sse_chunk(
        socket,
        &sse_named_event(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                },
            }),
        ),
    )
    .await?;
    write_sse_chunk(
        socket,
        &sse_named_event("message_stop", &json!({"type": "message_stop"})),
    )
    .await?;
    write_chunk_terminator(socket).await?;
    Ok(aggregated)
}
/// Anthropic content-block kind tracked by [`AnthropicBlockState`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AnthropicBlockKind {
    Text,
    Thinking,
}

/// Lazy block-opener for `stream_anthropic_sse`. Tracks the current block's
/// index and type so cursor's interleaved messages and thoughts get rendered
/// as alternating `text` and `thinking` blocks. Each transition closes the
/// current block and starts a fresh one at the next index, which is what
/// Anthropic's protocol requires (and what Claude Code's UI uses to pick
/// between the "Cogitated…" panel and the inline message bubble).
#[derive(Default)]
pub(super) struct AnthropicBlockState {
    pub(super) current: Option<(u32, AnthropicBlockKind)>,
    pub(super) next_index: u32,
}

impl AnthropicBlockState {
    pub(super) fn index(&self) -> u32 {
        self.current.map(|(i, _)| i).unwrap_or(0)
    }

    async fn ensure_kind(
        &mut self,
        socket: &mut TcpStream,
        kind: AnthropicBlockKind,
    ) -> Result<()> {
        if let Some((_, current)) = self.current
            && current == kind
        {
            return Ok(());
        }
        if let Some((idx, _)) = self.current.take() {
            write_sse_chunk(socket, &anthropic_content_block_stop(idx)).await?;
        }
        let idx = self.next_index;
        self.next_index += 1;
        write_sse_chunk(socket, &anthropic_content_block_start(idx, kind)).await?;
        self.current = Some((idx, kind));
        Ok(())
    }

    async fn close(&mut self, socket: &mut TcpStream) -> Result<()> {
        if let Some((idx, _)) = self.current.take() {
            write_sse_chunk(socket, &anthropic_content_block_stop(idx)).await?;
        }
        Ok(())
    }

    /// Reserve a content-block index without opening a `text`/`thinking`
    /// block. Used by the MCP-bridged path to emit a `tool_use` block,
    /// which has its own `content_block_start` shape that
    /// [`Self::ensure_kind`] can't produce.
    fn allocate_index(&mut self) -> u32 {
        let idx = self.next_index;
        self.next_index += 1;
        idx
    }
}

pub(super) fn anthropic_content_block_start(index: u32, kind: AnthropicBlockKind) -> String {
    let body = match kind {
        AnthropicBlockKind::Text => json!({"type": "text", "text": ""}),
        AnthropicBlockKind::Thinking => json!({"type": "thinking", "thinking": ""}),
    };
    sse_named_event(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": index,
            "content_block": body,
        }),
    )
}

pub(super) fn anthropic_content_block_stop(index: u32) -> String {
    sse_named_event(
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": index}),
    )
}

pub(super) fn anthropic_text_delta(index: u32, text: &str) -> String {
    sse_named_event(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "text_delta", "text": text},
        }),
    )
}

pub(super) fn anthropic_thinking_delta(index: u32, thinking: &str) -> String {
    sse_named_event(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "thinking_delta", "thinking": thinking},
        }),
    )
}

pub(super) fn anthropic_message_body(
    turn: &AggregatedTurn,
    model: &str,
    input_tokens: u64,
) -> Value {
    let mut content_blocks = Vec::new();
    if !turn.content.is_empty() {
        content_blocks.push(json!({"type": "text", "text": turn.content}));
    }
    json!({
        "id": new_anthropic_message_id(),
        "type": "message",
        "role": "assistant",
        "content": content_blocks,
        "model": model,
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": estimate_tokens(&turn.content),
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
        },
    })
}

/// Reduces an Anthropic `/v1/messages` request body to a flat ACP prompt.
/// Preserves `tools` as an "Available tools" header and surfaces `tool_use` /
/// `tool_result` blocks as transcript markers so multi-turn tool loops keep
/// their context when forwarded to Cursor.
pub(crate) fn reduce_anthropic_request_to_prompt(body: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(tools) = body.get("tools").and_then(Value::as_array)
        && let Some(block) = format_anthropic_tools_list(tools)
    {
        parts.push(block);
    }
    let system_text = extract_anthropic_system_text(body.get("system"));
    if !system_text.trim().is_empty() {
        parts.push(format!("System: {system_text}"));
    }
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return parts.join("\n\n");
    };
    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
        let label = match role {
            "user" => "User",
            "assistant" => "Assistant",
            other => other,
        };
        for entry in flatten_anthropic_message_blocks(label, msg.get("content")) {
            parts.push(entry);
        }
    }
    parts.join("\n\n")
}

/// Anthropic accepts `system` as a string or an array of text-typed blocks.
pub(super) fn extract_anthropic_system_text(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    match value {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut acc = String::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(t) = block.get("text").and_then(Value::as_str)
                {
                    if !acc.is_empty() {
                        acc.push('\n');
                    }
                    acc.push_str(t);
                }
            }
            acc
        }
        _ => String::new(),
    }
}

/// Walks one Anthropic message and yields a transcript line per logical block.
/// Plain text accumulates under `User:` / `Assistant:`; tool_use / tool_result
/// blocks become their own entries so the downstream agent sees the loop.
pub(super) fn flatten_anthropic_message_blocks(
    label: &str,
    content: Option<&Value>,
) -> Vec<String> {
    let mut out = Vec::new();
    let Some(content) = content else {
        return out;
    };
    let mut buffer = String::new();
    let flush = |buf: &mut String, out: &mut Vec<String>| {
        if !buf.trim().is_empty() {
            out.push(format!("{label}: {buf}"));
        }
        buf.clear();
    };
    match content {
        Value::String(s) if !s.trim().is_empty() => {
            out.push(format!("{label}: {s}"));
        }
        Value::Array(blocks) => {
            for block in blocks {
                let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "text" => {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            if !buffer.is_empty() {
                                buffer.push('\n');
                            }
                            buffer.push_str(t);
                        }
                    }
                    "tool_use" => {
                        flush(&mut buffer, &mut out);
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let args = block.get("input").cloned().unwrap_or(Value::Null);
                        out.push(format_tool_call_line(name, &args));
                    }
                    "tool_result" => {
                        flush(&mut buffer, &mut out);
                        let name = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("tool");
                        let result_text = extract_anthropic_tool_result_text(block.get("content"));
                        out.push(format_tool_result_block(name, &result_text));
                    }
                    "image" | "document" => {
                        flush(&mut buffer, &mut out);
                        out.push(format!("[{kind} attachment omitted]"));
                    }
                    _ => {}
                }
            }
            flush(&mut buffer, &mut out);
        }
        _ => {}
    }
    out
}

pub(super) fn extract_anthropic_tool_result_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut acc = String::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(t) = block.get("text").and_then(Value::as_str)
                {
                    if !acc.is_empty() {
                        acc.push('\n');
                    }
                    acc.push_str(t);
                }
            }
            acc
        }
        _ => String::new(),
    }
}

pub(super) fn new_anthropic_message_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let salt = current_unix_timestamp_micros();
    format!("msg_cur{n:x}{salt:x}")
}
