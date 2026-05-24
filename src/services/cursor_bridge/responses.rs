//! OpenAI Responses API (`/v1/responses`) adapter for cursor-backed codex.
//! Translates Responses-shaped requests into cursor-agent ACP prompts and
//! streams cursor's responses back as Responses-format SSE
//! (`response.output_item.*`, reasoning/message items). Tool-using turns
//! route through the [`super::mcp`] bridge with codex-style `call_*` IDs.

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

use super::anthropic::new_anthropic_message_id;
use super::mcp::{BridgeEvent, BridgeSession, ToolUseIdStyle};
use super::*;

// === Responses API (Codex) ===

pub(super) async fn handle_responses(
    socket: &mut TcpStream,
    state: &RouterState,
    request: &str,
) -> (u16, Option<String>) {
    match run_responses(socket, state, request).await {
        Ok(summary) => (200, summary),
        Err(err) => {
            let status = status_for_handler_error(&err);
            let msg = err.to_string();
            let _ = write_json_error(socket, status, &msg).await;
            (status, Some(msg))
        }
    }
}

pub(super) async fn run_responses(
    socket: &mut TcpStream,
    state: &RouterState,
    request: &str,
) -> Result<Option<String>> {
    let body_str = extract_request_body(request).context("read request body")?;
    let body: Value = serde_json::from_str(body_str).context("parse Responses request body")?;
    let stream_flag = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);

    // Non-streaming resumption: drain any parked call before falling
    // through to the legacy path (see Anthropic equivalent for rationale).
    if !stream_flag && let Some((call_id, output)) = extract_last_function_call_output(&body) {
        state
            .mcp_bridge
            .deliver_and_drop_parked(
                &call_id,
                vec![json!({"type": "text", "text": output})],
                false,
            )
            .await;
    }

    // Same gate as the Anthropic path: tools + streaming → bridge route.
    // Non-tool turns and non-streaming requests fall back to the existing
    // flat-text-prompt flow.
    if stream_flag && responses_request_uses_tools(&body) {
        return run_responses_bridged(socket, state, body, requested_model).await;
    }

    let parsed = ParsedTurn {
        stream_flag,
        requested_model,
        image_blocks: extract_responses_image_blocks(&body)?,
        prompt: reduce_responses_request_to_prompt(&body),
    };
    if parsed.prompt.trim().is_empty() && parsed.image_blocks.is_empty() {
        return Err(anyhow!("reduced prompt is empty; no user-visible message"));
    }
    run_turn(
        socket,
        state,
        parsed,
        CURSOR_ACP_SENTINEL,
        stream_responses_sse,
        responses_completion_body,
    )
    .await
}

// === Responses (codex) /responses with MCP-bridged client tools ===

pub(super) fn responses_request_uses_tools(body: &Value) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty())
}

/// Convert codex's Responses-API tool schemas (`{type: "function", name,
/// description, parameters}`) into the normalized `{name, description,
/// input_schema}` shape the [`McpBridge`] expects. Tolerates both the flat
/// Responses shape and the OpenAI-chat nested `{type: "function", function:
/// {...}}` shape so the same helper covers both call sites if we wire chat
/// later.
pub(super) fn extract_responses_tools_normalized(body: &Value) -> Vec<Value> {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(tools.len());
    for tool in tools {
        let inner = tool.get("function").unwrap_or(tool);
        let Some(name) = inner.get("name").and_then(Value::as_str) else {
            continue;
        };
        let description = inner
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let schema = inner
            .get("parameters")
            .or_else(|| inner.get("input_schema"))
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"}));
        out.push(json!({
            "name": name,
            "description": description,
            "input_schema": schema,
        }));
    }
    out
}

/// Walk `input` items in reverse and return the latest `function_call_output`
/// as `(call_id, output_string)`. Codex's tool-result shape uses a string
/// payload directly (unlike Anthropic's structured content blocks).
pub(super) fn extract_last_function_call_output(body: &Value) -> Option<(String, String)> {
    let items = body.get("input")?.as_array()?;
    for item in items.iter().rev() {
        if item.get("type").and_then(Value::as_str)? != "function_call_output" {
            continue;
        }
        let call_id = item.get("call_id").and_then(Value::as_str)?.to_string();
        let output = item
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        return Some((call_id, output));
    }
    None
}

/// Variant of [`reduce_responses_request_to_prompt`] used by the bridged
/// path: skips the `Available tools:` header (tools propagate via the MCP
/// server now) but keeps the input-item walk so the cursor model sees prior
/// tool loops in the conversation.
pub(super) fn reduce_responses_request_to_prompt_without_tools(body: &Value) -> String {
    let mut parts = Vec::new();
    let instructions = body
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !instructions.trim().is_empty() {
        parts.push(format!("System: {instructions}"));
    }
    match body.get("input") {
        Some(Value::String(s)) if !s.trim().is_empty() => {
            parts.push(format!("User: {s}"));
        }
        Some(Value::Array(items)) => {
            for item in items {
                let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
                match item_type {
                    "function_call" => {
                        let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let args = item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .map(parse_loose_json)
                            .unwrap_or(Value::Null);
                        parts.push(format_tool_call_line(name, &args));
                    }
                    "function_call_output" => {
                        let name = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or("tool");
                        let output = item.get("output").and_then(Value::as_str).unwrap_or("");
                        parts.push(format_tool_result_block(name, output));
                    }
                    "reasoning" => {}
                    "message" | "" => {
                        let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                        let label = match role {
                            "system" | "developer" => "System",
                            "user" => "User",
                            "assistant" => "Assistant",
                            other => other,
                        };
                        let text = extract_responses_item_text(item.get("content"));
                        if !text.trim().is_empty() {
                            parts.push(format!("{label}: {text}"));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    parts.join("\n\n")
}

pub(super) async fn run_responses_bridged(
    socket: &mut TcpStream,
    state: &RouterState,
    body: Value,
    requested_model: Option<String>,
) -> Result<Option<String>> {
    if let Some((call_id, output)) = extract_last_function_call_output(&body)
        && let Some(session) = state
            .mcp_bridge
            .resume_with_tool_result(
                &call_id,
                vec![json!({"type": "text", "text": output})],
                // Responses' function_call_output has no is_error field;
                // failures flow as text in `output`. Same caveat as the
                // OpenAI chat path — defer interpretation to the model.
                false,
            )
            .await
    {
        return run_responses_bridged_resume(socket, state, session, &body, requested_model).await;
    }
    run_responses_bridged_fresh(socket, state, body, requested_model).await
}

pub(super) async fn run_responses_bridged_fresh(
    socket: &mut TcpStream,
    state: &RouterState,
    body: Value,
    requested_model: Option<String>,
) -> Result<Option<String>> {
    let tools = extract_responses_tools_normalized(&body);
    let image_blocks = extract_responses_image_blocks(&body)?;
    let prompt = reduce_responses_request_to_prompt_without_tools(&body);
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
            .open_session(tools, ToolUseIdStyle::OpenAi)
            .await;
        let bridge_id = { bridge_session.lock().await.id.clone() };

        let acp_result = CursorAcpSession::open_with_mcp(
            &state.config.key,
            requested_model.as_deref(),
            &state.config.workspace_cwd,
            Some(&mcp_url),
        )
        .await
        .context("open cursor-agent ACP session with MCP bridge (responses)");

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

    stream_bridged_responses_turn(
        socket,
        state,
        bridge_session,
        &bridge_id,
        &response_model,
        input_tokens,
    )
    .await
}

pub(super) async fn run_responses_bridged_resume(
    socket: &mut TcpStream,
    state: &RouterState,
    bridge_session: Arc<tokio::sync::Mutex<BridgeSession>>,
    body: &Value,
    requested_model: Option<String>,
) -> Result<Option<String>> {
    let bridge_id = { bridge_session.lock().await.id.clone() };
    let input_tokens = estimate_tokens(&reduce_responses_request_to_prompt_without_tools(body));
    let response_model = requested_model.unwrap_or_else(|| CURSOR_ACP_SENTINEL.to_string());
    stream_bridged_responses_turn(
        socket,
        state,
        bridge_session,
        &bridge_id,
        &response_model,
        input_tokens,
    )
    .await
}

pub(super) async fn stream_bridged_responses_turn(
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
        {
            let mut guard = bridge_session.lock().await;
            guard.detach_event_sink();
        }
        drop(acp);
        drop(stream);
        state.mcp_bridge.drop_session(bridge_id).await;
        return Err(e).context("write Responses SSE head");
    }

    let resp_id = new_responses_id();
    let created = current_unix_timestamp();

    let _ = write_sse_chunk(
        socket,
        &sse_named_event(
            "response.created",
            &json!({
                "type": "response.created",
                "response": {
                    "id": resp_id,
                    "object": "response",
                    "model": response_model,
                    "created_at": created,
                    "status": "in_progress",
                    "output": [],
                },
            }),
        ),
    )
    .await;
    // Reasoning item is pre-opened at output_index 0 IMMEDIATELY so codex's
    // UI commits to rendering the reasoning panel before any text arrives.
    // Without this, when a turn starts with text (vs thought/tool), codex
    // anchors on the message item and intermittently hides reasoning that
    // streams in later — visible in round 2 of debug-20260524-114900.
    // Message item opens lazily at the next index on first text delta.
    let reasoning_id = new_responses_reasoning_id();
    let reasoning_index: u32 = 0;
    let _ = emit_responses_reasoning_open(socket, &resp_id, &reasoning_id, reasoning_index).await;
    let mut reasoning_text = String::new();
    let mut reasoning_closed = false;
    let mut current_message: Option<(String, String, u32)> = None;
    let mut next_output_index: u32 = 1;
    let mut output_items: Vec<Value> = Vec::new();
    let mut function_call_item: Option<Value> = None;
    let mut errored = false;
    let mut parked = false;

    'outer: loop {
        tokio::select! {
            biased;
            ev = event_rx.recv() => {
                match ev {
                    Some(BridgeEvent::ToolCall { tool_use_id, name, arguments }) => {
                        // Close reasoning + any open message block before
                        // opening the function_call output item.
                        if !reasoning_closed {
                            let _ = emit_responses_reasoning_close(
                                socket, &resp_id, &reasoning_id, reasoning_index, &reasoning_text,
                            ).await;
                            output_items.push(reasoning_item_done(&reasoning_id, &reasoning_text));
                            reasoning_closed = true;
                        }
                        if let Some((msg_id, msg_text, msg_idx)) = current_message.take() {
                            let _ = emit_responses_message_close(
                                socket, &resp_id, &msg_id, msg_idx, &msg_text,
                            ).await;
                            output_items.push(message_item_done(&msg_id, &msg_text));
                        }
                        // No `next_output_index += 1` here — we break out
                        // of the loop immediately after the function_call
                        // item closes.
                        let output_index = next_output_index;
                        let args_json = arguments.to_string();
                        let _ = write_sse_chunk(
                            socket,
                            &sse_named_event(
                                "response.output_item.added",
                                &json!({
                                    "type": "response.output_item.added",
                                    "response_id": resp_id,
                                    "output_index": output_index,
                                    "item": {
                                        "id": tool_use_id,
                                        "type": "function_call",
                                        "status": "in_progress",
                                        "call_id": tool_use_id,
                                        "name": name,
                                        "arguments": "",
                                    },
                                }),
                            ),
                        )
                        .await;
                        let _ = write_sse_chunk(
                            socket,
                            &sse_named_event(
                                "response.function_call_arguments.delta",
                                &json!({
                                    "type": "response.function_call_arguments.delta",
                                    "response_id": resp_id,
                                    "item_id": tool_use_id,
                                    "output_index": output_index,
                                    "delta": args_json,
                                }),
                            ),
                        )
                        .await;
                        let _ = write_sse_chunk(
                            socket,
                            &sse_named_event(
                                "response.function_call_arguments.done",
                                &json!({
                                    "type": "response.function_call_arguments.done",
                                    "response_id": resp_id,
                                    "item_id": tool_use_id,
                                    "output_index": output_index,
                                    "arguments": args_json,
                                }),
                            ),
                        )
                        .await;
                        let final_item = json!({
                            "id": tool_use_id,
                            "type": "function_call",
                            "status": "completed",
                            "call_id": tool_use_id,
                            "name": name,
                            "arguments": args_json,
                        });
                        let _ = write_sse_chunk(
                            socket,
                            &sse_named_event(
                                "response.output_item.done",
                                &json!({
                                    "type": "response.output_item.done",
                                    "response_id": resp_id,
                                    "output_index": output_index,
                                    "item": final_item.clone(),
                                }),
                            ),
                        )
                        .await;
                        function_call_item = Some(final_item);
                        parked = true;
                        break 'outer;
                    }
                    None => break 'outer,
                }
            }
            ev = stream.next() => {
                match ev {
                    Some(PromptEvent::Update(value)) => {
                        if let Some(text) = extract_agent_text(&value) {
                            // Message text → into the message item.
                            if current_message.is_none() {
                                let id = new_anthropic_message_id();
                                let idx = next_output_index;
                                next_output_index += 1;
                                let _ = emit_responses_message_open(
                                    socket, &resp_id, &id, idx,
                                ).await;
                                current_message = Some((id, String::new(), idx));
                            }
                            let entry = current_message.as_mut().unwrap();
                            let msg_id_str = entry.0.clone();
                            let msg_idx = entry.2;
                            entry.1.push_str(text);
                            let _ = write_sse_chunk(
                                socket,
                                &sse_named_event(
                                    "response.output_text.delta",
                                    &json!({
                                        "type": "response.output_text.delta",
                                        "response_id": resp_id,
                                        "item_id": msg_id_str,
                                        "output_index": msg_idx,
                                        "content_index": 0,
                                        "delta": text,
                                    }),
                                ),
                            )
                            .await;
                        } else if let Some(reasoning) = extract_agent_thought(&value)
                            .map(str::to_string)
                            .or_else(|| extract_tool_call_marker(&value))
                        {
                            // Thoughts and tool markers → into the pre-opened
                            // reasoning item. Even if reasoning streams while
                            // a message is also open, both items have unique
                            // ids so deltas route correctly.
                            if !reasoning_closed {
                                reasoning_text.push_str(&reasoning);
                                let _ = write_sse_chunk(
                                    socket,
                                    &sse_named_event(
                                        "response.reasoning_summary_text.delta",
                                        &json!({
                                            "type": "response.reasoning_summary_text.delta",
                                            "response_id": resp_id,
                                            "item_id": reasoning_id,
                                            "output_index": reasoning_index,
                                            "summary_index": 0,
                                            "delta": reasoning,
                                        }),
                                    ),
                                )
                                .await;
                            }
                        } else {
                            // Keep the stream alive on updates we don't
                            // surface (plans, available_commands, etc.) so
                            // OpenAI SDK clients don't time out.
                            let _ = write_sse_chunk(socket, SSE_KEEPALIVE).await;
                        }
                    }
                    Some(PromptEvent::Done(result)) => {
                        if result.is_err() {
                            errored = true;
                        }
                        break 'outer;
                    }
                    None => break 'outer,
                }
            }
        }
    }

    if errored && !parked {
        let _ = acp.cancel().await;
    }

    // Close the pre-opened reasoning item (always, even if empty — codex
    // saw the output_item.added at the top so it expects a matching done).
    if !reasoning_closed {
        let _ = emit_responses_reasoning_close(
            socket,
            &resp_id,
            &reasoning_id,
            reasoning_index,
            &reasoning_text,
        )
        .await;
        output_items.push(reasoning_item_done(&reasoning_id, &reasoning_text));
    }
    let final_message_text = if let Some((msg_id, msg_text, msg_idx)) = current_message.take() {
        let _ = emit_responses_message_close(socket, &resp_id, &msg_id, msg_idx, &msg_text).await;
        output_items.push(message_item_done(&msg_id, &msg_text));
        msg_text
    } else {
        String::new()
    };
    if let Some(item) = function_call_item.clone() {
        output_items.push(item);
    }
    let output_tokens: u64 = output_items
        .iter()
        .filter_map(|item| {
            item.get("content")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .and_then(|p| p.get("text"))
                .and_then(Value::as_str)
                .map(estimate_tokens)
        })
        .sum();
    let final_status = if errored { "failed" } else { "completed" };
    let _ = write_sse_chunk(
        socket,
        &sse_named_event(
            "response.completed",
            &json!({
                "type": "response.completed",
                "response": {
                    "id": resp_id,
                    "object": "response",
                    "model": response_model,
                    "created_at": created,
                    "status": final_status,
                    "output": output_items,
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": output_tokens,
                        "total_tokens": input_tokens.saturating_add(output_tokens),
                    },
                },
            }),
        ),
    )
    .await;
    let _ = write_chunk_terminator(socket).await;

    {
        let mut guard = bridge_session.lock().await;
        guard.detach_event_sink();
        if parked {
            guard.return_active(acp, stream);
        } else {
            drop(acp);
            drop(stream);
        }
    }
    if !parked {
        state.mcp_bridge.drop_session(bridge_id).await;
    }

    Ok(if final_message_text.is_empty() {
        None
    } else {
        Some(final_message_text)
    })
}

pub(super) fn new_responses_reasoning_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let salt = current_unix_timestamp_micros();
    format!("rs_cur{n:x}{salt:x}")
}

pub(super) fn message_item_done(msg_id: &str, full_text: &str) -> Value {
    json!({
        "id": msg_id,
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": full_text, "annotations": []}],
    })
}

pub(super) fn reasoning_item_done(rs_id: &str, full_text: &str) -> Value {
    json!({
        "id": rs_id,
        "type": "reasoning",
        "summary": [{"type": "summary_text", "text": full_text}],
    })
}

pub(super) async fn emit_responses_reasoning_open(
    socket: &mut TcpStream,
    resp_id: &str,
    rs_id: &str,
    output_index: u32,
) -> Result<()> {
    write_sse_chunk(
        socket,
        &sse_named_event(
            "response.output_item.added",
            &json!({
                "type": "response.output_item.added",
                "response_id": resp_id,
                "output_index": output_index,
                "item": {
                    "id": rs_id,
                    "type": "reasoning",
                    "summary": [],
                },
            }),
        ),
    )
    .await?;
    write_sse_chunk(
        socket,
        &sse_named_event(
            "response.reasoning_summary_part.added",
            &json!({
                "type": "response.reasoning_summary_part.added",
                "response_id": resp_id,
                "item_id": rs_id,
                "output_index": output_index,
                "summary_index": 0,
                "part": {"type": "summary_text", "text": ""},
            }),
        ),
    )
    .await?;
    Ok(())
}

pub(super) async fn emit_responses_reasoning_close(
    socket: &mut TcpStream,
    resp_id: &str,
    rs_id: &str,
    output_index: u32,
    full_text: &str,
) -> Result<()> {
    write_sse_chunk(
        socket,
        &sse_named_event(
            "response.reasoning_summary_text.done",
            &json!({
                "type": "response.reasoning_summary_text.done",
                "response_id": resp_id,
                "item_id": rs_id,
                "output_index": output_index,
                "summary_index": 0,
                "text": full_text,
            }),
        ),
    )
    .await?;
    write_sse_chunk(
        socket,
        &sse_named_event(
            "response.reasoning_summary_part.done",
            &json!({
                "type": "response.reasoning_summary_part.done",
                "response_id": resp_id,
                "item_id": rs_id,
                "output_index": output_index,
                "summary_index": 0,
                "part": {"type": "summary_text", "text": full_text},
            }),
        ),
    )
    .await?;
    write_sse_chunk(
        socket,
        &sse_named_event(
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "response_id": resp_id,
                "output_index": output_index,
                "item": reasoning_item_done(rs_id, full_text),
            }),
        ),
    )
    .await?;
    Ok(())
}

pub(super) async fn emit_responses_message_open(
    socket: &mut TcpStream,
    resp_id: &str,
    msg_id: &str,
    output_index: u32,
) -> Result<()> {
    write_sse_chunk(
        socket,
        &sse_named_event(
            "response.output_item.added",
            &json!({
                "type": "response.output_item.added",
                "response_id": resp_id,
                "output_index": output_index,
                "item": {
                    "id": msg_id,
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": [],
                },
            }),
        ),
    )
    .await?;
    write_sse_chunk(
        socket,
        &sse_named_event(
            "response.content_part.added",
            &json!({
                "type": "response.content_part.added",
                "response_id": resp_id,
                "item_id": msg_id,
                "output_index": output_index,
                "content_index": 0,
                "part": {"type": "output_text", "text": ""},
            }),
        ),
    )
    .await?;
    Ok(())
}

pub(super) async fn emit_responses_message_close(
    socket: &mut TcpStream,
    resp_id: &str,
    msg_id: &str,
    output_index: u32,
    full_text: &str,
) -> Result<()> {
    write_sse_chunk(
        socket,
        &sse_named_event(
            "response.output_text.done",
            &json!({
                "type": "response.output_text.done",
                "response_id": resp_id,
                "item_id": msg_id,
                "output_index": output_index,
                "content_index": 0,
                "text": full_text,
            }),
        ),
    )
    .await?;
    write_sse_chunk(
        socket,
        &sse_named_event(
            "response.content_part.done",
            &json!({
                "type": "response.content_part.done",
                "response_id": resp_id,
                "item_id": msg_id,
                "output_index": output_index,
                "content_index": 0,
                "part": {"type": "output_text", "text": full_text, "annotations": []},
            }),
        ),
    )
    .await?;
    write_sse_chunk(
        socket,
        &sse_named_event(
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "response_id": resp_id,
                "output_index": output_index,
                "item": {
                    "id": msg_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": full_text, "annotations": []}],
                },
            }),
        ),
    )
    .await?;
    Ok(())
}

pub(super) async fn stream_responses_sse(
    socket: &mut TcpStream,
    stream: &mut crate::services::acp_client::PromptStream,
    model: &str,
    input_tokens: u64,
) -> Result<String> {
    let head = http_chunked_response_head_with_extra(200, "text/event-stream", cors_header_block());
    socket.write_all(head.as_bytes()).await?;

    let resp_id = new_responses_id();
    let msg_id = new_anthropic_message_id();
    let created = current_unix_timestamp();

    write_sse_chunk(
        socket,
        &sse_named_event(
            "response.created",
            &json!({
                "type": "response.created",
                "response": {
                    "id": resp_id,
                    "object": "response",
                    "model": model,
                    "created_at": created,
                    "status": "in_progress",
                    "output": [],
                },
            }),
        ),
    )
    .await?;
    write_sse_chunk(
        socket,
        &sse_named_event(
            "response.output_item.added",
            &json!({
                "type": "response.output_item.added",
                "response_id": resp_id,
                "output_index": 0,
                "item": {
                    "id": msg_id,
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": [],
                },
            }),
        ),
    )
    .await?;
    write_sse_chunk(
        socket,
        &sse_named_event(
            "response.content_part.added",
            &json!({
                "type": "response.content_part.added",
                "response_id": resp_id,
                "item_id": msg_id,
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": ""},
            }),
        ),
    )
    .await?;

    let mut full_text = String::new();
    let mut errored = false;
    while let Some(event) = stream.next().await {
        match event {
            PromptEvent::Update(value) => {
                if let Some(text) = extract_agent_text(&value) {
                    full_text.push_str(text);
                    write_sse_chunk(
                        socket,
                        &sse_named_event(
                            "response.output_text.delta",
                            &json!({
                                "type": "response.output_text.delta",
                                "response_id": resp_id,
                                "item_id": msg_id,
                                "output_index": 0,
                                "content_index": 0,
                                "delta": text,
                            }),
                        ),
                    )
                    .await?;
                } else {
                    // Keep the stream alive during non-text updates; see the
                    // OpenAI streamer for the rationale.
                    write_sse_chunk(socket, SSE_KEEPALIVE).await?;
                }
            }
            PromptEvent::Done(result) => {
                if result.is_err() {
                    errored = true;
                }
                break;
            }
        }
    }

    write_sse_chunk(
        socket,
        &sse_named_event(
            "response.output_text.done",
            &json!({
                "type": "response.output_text.done",
                "response_id": resp_id,
                "item_id": msg_id,
                "output_index": 0,
                "content_index": 0,
                "text": full_text,
            }),
        ),
    )
    .await?;
    write_sse_chunk(
        socket,
        &sse_named_event(
            "response.content_part.done",
            &json!({
                "type": "response.content_part.done",
                "response_id": resp_id,
                "item_id": msg_id,
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": full_text},
            }),
        ),
    )
    .await?;
    write_sse_chunk(
        socket,
        &sse_named_event(
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "response_id": resp_id,
                "output_index": 0,
                "item": {
                    "id": msg_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": full_text, "annotations": []}],
                },
            }),
        ),
    )
    .await?;
    let final_status = if errored { "failed" } else { "completed" };
    write_sse_chunk(
        socket,
        &sse_named_event(
            "response.completed",
            &json!({
                "type": "response.completed",
                "response": {
                    "id": resp_id,
                    "object": "response",
                    "model": model,
                    "created_at": created,
                    "status": final_status,
                    "output": [{
                        "id": msg_id,
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": full_text, "annotations": []}],
                    }],
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": estimate_tokens(&full_text),
                        "total_tokens": input_tokens.saturating_add(estimate_tokens(&full_text)),
                    },
                },
            }),
        ),
    )
    .await?;
    write_chunk_terminator(socket).await?;
    Ok(full_text)
}

pub(super) fn responses_completion_body(
    turn: &AggregatedTurn,
    model: &str,
    input_tokens: u64,
) -> Value {
    let msg_id = new_anthropic_message_id();
    let output_tokens = estimate_tokens(&turn.content);
    json!({
        "id": new_responses_id(),
        "object": "response",
        "created_at": current_unix_timestamp(),
        "model": model,
        "status": "completed",
        "output": [{
            "id": msg_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": turn.content, "annotations": []}],
        }],
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "total_tokens": input_tokens.saturating_add(output_tokens),
        },
    })
}

/// Reduces a Responses-API request body to a flat ACP prompt. Honors the
/// top-level `instructions` field as a system prefix, formats the `tools`
/// schema list, and walks the `input` array's typed items — including
/// `function_call` / `function_call_output` — so Codex tool loops keep their
/// context when forwarded to Cursor.
pub(crate) fn reduce_responses_request_to_prompt(body: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(tools) = body.get("tools").and_then(Value::as_array)
        && let Some(block) = format_responses_tools_list(tools)
    {
        parts.push(block);
    }
    let instructions = body
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !instructions.trim().is_empty() {
        parts.push(format!("System: {instructions}"));
    }
    match body.get("input") {
        Some(Value::String(s)) if !s.trim().is_empty() => {
            parts.push(format!("User: {s}"));
        }
        Some(Value::Array(items)) => {
            for item in items {
                let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
                match item_type {
                    "function_call" => {
                        let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let args = item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .map(parse_loose_json)
                            .unwrap_or(Value::Null);
                        parts.push(format_tool_call_line(name, &args));
                    }
                    "function_call_output" => {
                        let name = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or("tool");
                        let output = item.get("output").and_then(Value::as_str).unwrap_or("");
                        parts.push(format_tool_result_block(name, output));
                    }
                    "reasoning" => {
                        // Codex emits its own chain-of-thought summary; drop it
                        // to keep prompts small. Cursor will produce its own.
                    }
                    "message" | "" => {
                        let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                        let label = match role {
                            "system" | "developer" => "System",
                            "user" => "User",
                            "assistant" => "Assistant",
                            other => other,
                        };
                        let text = extract_responses_item_text(item.get("content"));
                        if !text.trim().is_empty() {
                            parts.push(format!("{label}: {text}"));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    parts.join("\n\n")
}

pub(super) fn extract_responses_item_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut acc = String::new();
            for block in blocks {
                let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
                if (kind == "input_text" || kind == "output_text" || kind == "text")
                    && let Some(text) = block.get("text").and_then(Value::as_str)
                {
                    if !acc.is_empty() {
                        acc.push('\n');
                    }
                    acc.push_str(text);
                }
            }
            acc
        }
        _ => String::new(),
    }
}
