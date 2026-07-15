use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

use crate::agent::ask::AskOption;
use crate::agent::engine::{AgentUi, ExternalApproval, TurnStop};
use crate::agent::plan::PlanItem;
use crate::agent::protocol::Decision;

use super::cloud_records::CloudRunSink;
use super::protocol;

pub type Outbound = UnboundedSender<Value>;
pub type PendingInteractions = Arc<Mutex<HashMap<String, PendingInteraction>>>;

pub struct PendingInteraction {
    pub thread_id: String,
    pub turn_id: String,
    reply: PendingReply,
}

enum PendingReply {
    Approval {
        reply: oneshot::Sender<Decision>,
        allow_always: bool,
    },
    UserInput(oneshot::Sender<Result<String, String>>),
}

#[derive(Clone)]
pub struct EventEmitter {
    outbound: Outbound,
    thread_id: String,
    turn_id: String,
    seq: Arc<AtomicU64>,
}

impl EventEmitter {
    pub fn new(
        outbound: Outbound,
        thread_id: String,
        turn_id: String,
        seq: Arc<AtomicU64>,
    ) -> Self {
        Self {
            outbound,
            thread_id,
            turn_id,
            seq,
        }
    }

    pub fn emit(&self, event_type: &str, payload: Value) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.outbound.send(protocol::event(
            &self.thread_id,
            &self.turn_id,
            seq,
            event_type,
            payload,
        ));
    }
}

pub struct AppServerUi {
    emitter: EventEmitter,
    outbound: Outbound,
    pending: PendingInteractions,
    request_seq: Arc<AtomicU64>,
    cloud: Option<CloudRunSink>,
    text_segment: String,
    reasoning_segment: String,
    answer: String,
    tool_seq: u64,
    pending_tools: VecDeque<(String, String)>,
    pub last_error: Option<String>,
    pub stopped: Option<TurnStop>,
}

impl AppServerUi {
    pub fn new(
        emitter: EventEmitter,
        outbound: Outbound,
        pending: PendingInteractions,
        request_seq: Arc<AtomicU64>,
        cloud: Option<CloudRunSink>,
    ) -> Self {
        Self {
            emitter,
            outbound,
            pending,
            request_seq,
            cloud,
            text_segment: String::new(),
            reasoning_segment: String::new(),
            answer: String::new(),
            tool_seq: 0,
            pending_tools: VecDeque::new(),
            last_error: None,
            stopped: None,
        }
    }

    pub fn finish_streams(&mut self) {
        self.flush_reasoning();
        self.flush_text();
    }

    pub fn answer(&self) -> &str {
        &self.answer
    }

    fn flush_text(&mut self) {
        if self.text_segment.is_empty() {
            return;
        }
        let text = redact(&self.text_segment);
        self.emitter
            .emit("assistant.text.delta", json!({ "text": text }));
        self.answer.push_str(&self.text_segment);
        self.text_segment.clear();
    }

    fn flush_reasoning(&mut self) {
        if self.reasoning_segment.is_empty() {
            return;
        }
        let text = redact(&self.reasoning_segment);
        self.emitter
            .emit("assistant.reasoning.delta", json!({ "text": text }));
        self.reasoning_segment.clear();
    }

    fn flush_streams(&mut self) {
        self.flush_reasoning();
        self.flush_text();
    }

    fn interaction_id(&self) -> String {
        let seq = self.request_seq.fetch_add(1, Ordering::Relaxed) + 1;
        format!("server:{seq}")
    }

    fn insert_pending(&self, id: String, reply: PendingReply) {
        self.pending.lock().unwrap().insert(
            id,
            PendingInteraction {
                thread_id: self.emitter.thread_id.clone(),
                turn_id: self.emitter.turn_id.clone(),
                reply,
            },
        );
    }
}

impl AgentUi for AppServerUi {
    fn turn_start(&mut self) {
        self.flush_streams();
    }

    fn assistant_text(&mut self, delta: &str) {
        self.text_segment.push_str(delta);
    }

    fn assistant_reasoning(&mut self, delta: &str) {
        self.reasoning_segment.push_str(delta);
    }

    fn discard_streamed_segment(&mut self) {
        self.text_segment.clear();
    }

    fn context_usage(&mut self, tokens: u64, measured: bool) {
        self.emitter.emit(
            "context.updated",
            json!({ "tokens": tokens, "measured": measured }),
        );
    }

    fn turn_tokens(&mut self, output: u64) {
        self.emitter
            .emit("usage.updated", json!({ "outputTokens": output }));
    }

    fn plan_updated(&mut self, items: &[PlanItem]) {
        self.flush_streams();
        self.emitter.emit(
            "plan.updated",
            json!({ "items": serde_json::to_value(items).unwrap_or(Value::Null) }),
        );
    }

    fn tool_start(&mut self, name: &str, args: &Value) {
        self.flush_streams();
        self.tool_seq += 1;
        let tool_call_id = format!("tool:{}", self.tool_seq);
        self.pending_tools
            .push_back((name.to_string(), tool_call_id.clone()));
        if let Some(cloud) = &self.cloud {
            cloud.audit(
                "tool.started",
                json!({ "tool_call_id": tool_call_id, "tool": name }),
            );
        }
        self.emitter.emit(
            "tool.started",
            json!({
                "toolCallId": tool_call_id,
                "name": name,
                "args": redact_value(args),
            }),
        );
    }

    fn tool_result(&mut self, name: &str, result: &Result<String, String>) {
        let position = self
            .pending_tools
            .iter()
            .position(|(pending_name, _)| pending_name == name);
        let tool_call_id = position
            .and_then(|index| self.pending_tools.remove(index))
            .map(|(_, id)| id)
            .unwrap_or_else(|| {
                self.tool_seq += 1;
                format!("tool:{}", self.tool_seq)
            });
        let payload = match result {
            Ok(output) => json!({
                "toolCallId": tool_call_id,
                "name": name,
                "ok": true,
                "output": redact(output),
            }),
            Err(error) => json!({
                "toolCallId": tool_call_id,
                "name": name,
                "ok": false,
                "error": redact(error),
            }),
        };
        if let Some(cloud) = &self.cloud {
            cloud.record_tool_result(name, result);
        }
        self.emitter.emit("tool.completed", payload);
    }

    fn notify(&mut self, text: &str) {
        self.flush_streams();
        self.emitter.emit("notice", json!({ "text": redact(text) }));
    }

    fn notify_error(&mut self, text: &str) {
        self.flush_streams();
        self.last_error = Some(text.to_string());
        self.emitter.emit("error", json!({ "text": redact(text) }));
    }

    fn turn_stopped(&mut self, stop: TurnStop) {
        self.stopped = Some(stop);
    }

    fn footer(
        &mut self,
        summary: Option<&str>,
        steps: usize,
        tokens: u64,
        context_tokens: u64,
        elapsed_secs: u64,
    ) {
        self.finish_streams();
        self.emitter.emit(
            "usage.updated",
            json!({
                "summary": summary.map(redact),
                "steps": steps,
                "tokens": tokens,
                "contextTokens": context_tokens,
                "elapsedSecs": elapsed_secs,
            }),
        );
    }

    fn ask_permission<'a>(
        &'a mut self,
        tool: &'a str,
        preview: Option<&'a str>,
    ) -> BoxFuture<'a, Decision> {
        self.flush_streams();
        let id = self.interaction_id();
        let (reply, rx) = oneshot::channel();
        self.insert_pending(
            id.clone(),
            PendingReply::Approval {
                reply,
                allow_always: true,
            },
        );
        let request = protocol::server_request(
            id.clone(),
            "approval/request",
            json!({
                "schemaVersion": protocol::EVENT_SCHEMA_VERSION,
                "threadId": self.emitter.thread_id,
                "turnId": self.emitter.turn_id,
                "kind": "tool",
                "subject": {
                    "tool": tool,
                    "preview": preview.map(redact),
                },
                "choices": ["allow", "deny", "always_allow"],
            }),
        );
        let sent = self.outbound.send(request).is_ok();
        let pending = self.pending.clone();
        Box::pin(async move {
            if !sent {
                pending.lock().unwrap().remove(&id);
                return Decision::Deny;
            }
            rx.await.unwrap_or(Decision::Deny)
        })
    }

    fn ask_external_permission<'a>(
        &'a mut self,
        tool: &'a str,
        preview: Option<&'a str>,
        approval: &'a ExternalApproval,
    ) -> BoxFuture<'a, Decision> {
        self.flush_streams();
        let id = self.interaction_id();
        if let Some(cloud) = &self.cloud {
            cloud.audit(
                "approval.requested",
                json!({
                    "tool": tool,
                    "effect": approval.effect,
                    "binding": approval.binding,
                    "fresh": approval.fresh,
                }),
            );
        }
        let (reply, rx) = oneshot::channel();
        self.insert_pending(
            id.clone(),
            PendingReply::Approval {
                reply,
                allow_always: approval.allow_always,
            },
        );
        let choices = if approval.allow_always {
            json!(["allow", "deny", "always_allow"])
        } else {
            json!(["allow", "deny"])
        };
        let request = protocol::server_request(
            id.clone(),
            "approval/request",
            json!({
                "schemaVersion": protocol::EVENT_SCHEMA_VERSION,
                "threadId": self.emitter.thread_id,
                "turnId": self.emitter.turn_id,
                "kind": "tool",
                "subject": {
                    "tool": tool,
                    "preview": preview.map(redact),
                    "effect": approval.effect,
                    "reason": redact(&approval.reason),
                    "target": redact_value(&approval.target),
                    "binding": approval.binding,
                    "fresh": approval.fresh,
                },
                "choices": choices,
            }),
        );
        let sent = self.outbound.send(request).is_ok();
        let pending = self.pending.clone();
        let cloud = self.cloud.clone();
        let tool = tool.to_string();
        let binding = approval.binding.clone();
        Box::pin(async move {
            if !sent {
                pending.lock().unwrap().remove(&id);
                return Decision::Deny;
            }
            let decision = rx.await.unwrap_or(Decision::Deny);
            if let Some(cloud) = cloud {
                cloud.audit(
                    "approval.resolved",
                    json!({
                        "tool": tool,
                        "binding": binding,
                        "decision": serde_json::to_value(decision).unwrap_or(Value::Null),
                    }),
                );
            }
            decision
        })
    }

    fn ask_user<'a>(
        &'a mut self,
        question: &'a str,
        options: &'a [AskOption],
        allow_free_text: bool,
        multi_select: bool,
    ) -> BoxFuture<'a, Result<String, String>> {
        self.flush_streams();
        let id = self.interaction_id();
        let (reply, rx) = oneshot::channel();
        self.insert_pending(id.clone(), PendingReply::UserInput(reply));
        let request = protocol::server_request(
            id.clone(),
            "userInput/request",
            json!({
                "schemaVersion": protocol::EVENT_SCHEMA_VERSION,
                "threadId": self.emitter.thread_id,
                "turnId": self.emitter.turn_id,
                "question": redact(question),
                "options": options,
                "allowFreeText": allow_free_text,
                "multiSelect": multi_select,
            }),
        );
        let sent = self.outbound.send(request).is_ok();
        let pending = self.pending.clone();
        Box::pin(async move {
            if !sent {
                pending.lock().unwrap().remove(&id);
                return Err("app-server client disconnected".to_string());
            }
            rx.await
                .unwrap_or_else(|_| Err("app-server interaction was cancelled".to_string()))
        })
    }
}

pub fn resolve_interaction_response(
    pending: &PendingInteractions,
    id: &Value,
    result: Option<&Value>,
    error: Option<&Value>,
) -> Result<bool, String> {
    let Some(id) = id.as_str() else {
        return Ok(false);
    };
    let Some(entry) = pending.lock().unwrap().remove(id) else {
        return Ok(false);
    };
    let error_message = error.map(|value| {
        value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("client rejected the interaction")
            .to_string()
    });
    match entry.reply {
        PendingReply::Approval {
            reply,
            allow_always,
        } => {
            let decision = if error_message.is_some() {
                Decision::Deny
            } else {
                match result
                    .and_then(|value| value.get("decision"))
                    .and_then(Value::as_str)
                {
                    Some("allow") => Decision::Allow,
                    Some("always_allow") if allow_always => Decision::AlwaysAllow,
                    // A stale or malicious protocol client cannot widen a
                    // fresh approval. Treat it as consent for this call only.
                    Some("always_allow") => Decision::Allow,
                    Some("deny") | None => Decision::Deny,
                    Some(other) => {
                        let _ = reply.send(Decision::Deny);
                        return Err(format!("unknown approval decision: {other}"));
                    }
                }
            };
            let _ = reply.send(decision);
        }
        PendingReply::UserInput(reply) => {
            let value = match error_message {
                Some(message) => Err(message),
                None => {
                    let answers = result
                        .and_then(|value| value.get("answers"))
                        .and_then(Value::as_array)
                        .map(|values| {
                            values
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .or_else(|| {
                            result
                                .and_then(|value| value.get("answer"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .filter(|answer| !answer.trim().is_empty());
                    answers.ok_or_else(|| "user input response has no answers".to_string())
                }
            };
            let _ = reply.send(value);
        }
    }
    Ok(true)
}

pub fn fail_pending_for_turn(pending: &PendingInteractions, thread_id: &str, turn_id: &str) {
    let ids = {
        let guard = pending.lock().unwrap();
        guard
            .iter()
            .filter(|(_, entry)| entry.thread_id == thread_id && entry.turn_id == turn_id)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>()
    };
    let mut guard = pending.lock().unwrap();
    for id in ids {
        if let Some(entry) = guard.remove(&id) {
            fail_interaction(entry);
        }
    }
}

pub fn fail_all_pending(pending: &PendingInteractions) {
    let entries = pending
        .lock()
        .unwrap()
        .drain()
        .map(|(_, entry)| entry)
        .collect::<Vec<_>>();
    for entry in entries {
        fail_interaction(entry);
    }
}

fn fail_interaction(entry: PendingInteraction) {
    match entry.reply {
        PendingReply::Approval { reply, .. } => {
            let _ = reply.send(Decision::Deny);
        }
        PendingReply::UserInput(reply) => {
            let _ = reply.send(Err("app-server interaction was cancelled".to_string()));
        }
    }
}

fn redact(text: &str) -> String {
    crate::agent::secrets_guard::redact_for_model(text).0
}

fn redact_value(value: &Value) -> Value {
    let (redacted, count) = crate::agent::secrets_guard::redact_for_model(&value.to_string());
    if count == 0 {
        return value.clone();
    }
    serde_json::from_str(&redacted).unwrap_or(Value::String(redacted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approval_response_round_trips() {
        let pending = PendingInteractions::default();
        let (reply, rx) = oneshot::channel();
        pending.lock().unwrap().insert(
            "server:1".into(),
            PendingInteraction {
                thread_id: "thr_1".into(),
                turn_id: "turn_1".into(),
                reply: PendingReply::Approval {
                    reply,
                    allow_always: true,
                },
            },
        );
        let handled = resolve_interaction_response(
            &pending,
            &json!("server:1"),
            Some(&json!({"decision": "always_allow"})),
            None,
        )
        .unwrap();
        assert!(handled);
        assert_eq!(rx.await.unwrap(), Decision::AlwaysAllow);
    }

    #[tokio::test]
    async fn cancelling_a_turn_fails_closed() {
        let pending = PendingInteractions::default();
        let (reply, rx) = oneshot::channel();
        pending.lock().unwrap().insert(
            "server:1".into(),
            PendingInteraction {
                thread_id: "thr_1".into(),
                turn_id: "turn_1".into(),
                reply: PendingReply::Approval {
                    reply,
                    allow_always: true,
                },
            },
        );
        fail_pending_for_turn(&pending, "thr_1", "turn_1");
        assert_eq!(rx.await.unwrap(), Decision::Deny);
    }

    #[test]
    fn structured_values_are_redacted_before_the_wire() {
        let value = json!({"token": "sk-abcdefghijklmnopqrstuvwxyz123456"});
        let redacted = redact_value(&value).to_string();
        assert!(!redacted.contains("sk-abcdefghijklmnopqrstuvwxyz123456"));
    }
}
