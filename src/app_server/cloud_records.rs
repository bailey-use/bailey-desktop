//! Best-effort Bailey Cloud run records for Desktop turns.
//!
//! This is deliberately an asynchronous side channel. The local AgentEngine is
//! authoritative and never waits for record writes after the worker is queued.
//! Only an allowlisted envelope leaves the machine: no cwd, prompts, tool
//! arguments, assistant text, screenshots, DOM, or local evidence paths.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::Method;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::agent::mcp::BAILEY_LOCAL_MCP_TOOL_PREFIX;

use super::ui::EventEmitter;

const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

#[derive(Clone)]
pub struct CloudRunSink {
    tx: mpsc::Sender<CloudCommand>,
    degraded: Arc<AtomicBool>,
    emitter: EventEmitter,
}

enum CloudCommand {
    Audit {
        event_type: String,
        data: Value,
    },
    Operation(Value),
    Evidence(Value),
    Finish {
        status: String,
        persisted_locally: bool,
    },
}

struct CloudConfig {
    base_url: String,
    token: Zeroizing<String>,
}

impl CloudRunSink {
    pub fn start(
        session_id: &str,
        thread_id: &str,
        turn_id: &str,
        model: &str,
        emitter: EventEmitter,
    ) -> Option<Self> {
        let config = CloudConfig::from_environment()?;
        let (tx, rx) = mpsc::channel(256);
        let degraded = Arc::new(AtomicBool::new(false));
        let worker_degraded = degraded.clone();
        let identity = json!({
            "caller_id": opaque_id(session_id),
            "metadata": {
                "desktop_version": std::env::var("BAILEY_DESKTOP_VERSION").unwrap_or_else(|_| "unknown".to_string()),
                "agent_runtime_version": crate::version::VERSION,
                "thread_ref": opaque_id(thread_id),
                "turn_ref": opaque_id(turn_id),
                "model": model,
                "redaction": "allowlist-v1",
            },
        });
        let worker_emitter = emitter.clone();
        tokio::spawn(async move {
            run_worker(config, identity, rx, worker_degraded, worker_emitter).await;
        });
        Some(Self {
            tx,
            degraded,
            emitter,
        })
    }

    pub fn audit(&self, event_type: &str, data: Value) {
        self.enqueue(CloudCommand::Audit {
            event_type: event_type.to_string(),
            data,
        });
    }

    pub fn record_tool_result(&self, name: &str, result: &Result<String, String>) {
        if !name.starts_with(BAILEY_LOCAL_MCP_TOOL_PREFIX) {
            self.audit(
                "tool.completed",
                json!({ "tool": name, "ok": result.is_ok() }),
            );
            return;
        }
        let operation = sanitized_operation(name, result);
        let evidence = evidence_summary(&operation);
        self.enqueue(CloudCommand::Operation(operation));
        if let Some(evidence) = evidence {
            self.enqueue(CloudCommand::Evidence(evidence));
        }
    }

    pub fn finish(&self, status: &str, persisted_locally: bool) {
        self.enqueue(CloudCommand::Finish {
            status: status.to_string(),
            persisted_locally,
        });
    }

    fn enqueue(&self, command: CloudCommand) {
        if self.tx.try_send(command).is_err() {
            mark_degraded(&self.degraded, &self.emitter);
        }
    }
}

impl CloudConfig {
    fn from_environment() -> Option<Self> {
        if std::env::var("BAILEY_DISABLE_CLOUD_RECORDS").as_deref() == Ok("1") {
            return None;
        }
        let token = std::env::var("BAILEY_CLOUD_RECORDS_API_KEY")
            .ok()
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())?;
        let base_url = std::env::var("BAILEY_CLOUD_RECORD_BASE_URL")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| crate::constants::BAILEY_CLOUD_RECORD_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        Some(Self {
            base_url,
            token: Zeroizing::new(token),
        })
    }
}

async fn run_worker(
    config: CloudConfig,
    identity: Value,
    mut rx: mpsc::Receiver<CloudCommand>,
    degraded: Arc<AtomicBool>,
    emitter: EventEmitter,
) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .user_agent(format!("bailey-desktop/{}", crate::version::VERSION))
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            mark_degraded(&degraded, &emitter);
            return;
        }
    };
    let create = json!({
        "goal": "Desktop agent turn",
        "status": "running",
        "caller_id": identity["caller_id"],
        "context": { "runtime": "bailey-desktop" },
        "inputs": {},
        "metadata": identity["metadata"],
        "source": "bailey-desktop",
    });
    let created = request_json(&client, &config, Method::POST, "/runs", &create).await;
    let Some(run_id) = created
        .ok()
        .and_then(|value| value.pointer("/run/run_id").and_then(Value::as_str).map(str::to_string))
    else {
        mark_degraded(&degraded, &emitter);
        return;
    };

    while let Some(command) = rx.recv().await {
        let request = match command {
            CloudCommand::Audit { event_type, data } => {
                let body = json!({
                    "event_type": event_type,
                    "source": "bailey-desktop",
                    "data": data,
                });
                request_json(
                    &client,
                    &config,
                    Method::POST,
                    &format!("/runs/{run_id}/events"),
                    &body,
                )
                .await
                .map(|_| false)
            }
            CloudCommand::Operation(operation) => {
                let body = json!({ "source": "bailey-desktop", "operation": operation });
                request_json(
                    &client,
                    &config,
                    Method::POST,
                    &format!("/runs/{run_id}/operations"),
                    &body,
                )
                .await
                .map(|_| false)
            }
            CloudCommand::Evidence(data) => {
                let body = json!({
                    "event_type": "evidence.recorded",
                    "source": "bailey-desktop",
                    "data": data,
                });
                request_json(
                    &client,
                    &config,
                    Method::POST,
                    &format!("/runs/{run_id}/events"),
                    &body,
                )
                .await
                .map(|_| false)
            }
            CloudCommand::Finish {
                status,
                persisted_locally,
            } => {
                let body = json!({
                    "source": "bailey-desktop",
                    "patch": {
                        "status": status,
                        "result": { "persisted_locally": persisted_locally },
                    },
                });
                request_json(
                    &client,
                    &config,
                    Method::PATCH,
                    &format!("/runs/{run_id}"),
                    &body,
                )
                .await
                .map(|_| true)
            }
        };
        match request {
            Ok(done) if done => return,
            Ok(_) => {}
            Err(_) => mark_degraded(&degraded, &emitter),
        }
    }

    // The sender disappears when a running turn is cancelled/aborted. Close
    // the remote record as interrupted instead of leaving it permanently live.
    let body = json!({
        "source": "bailey-desktop",
        "patch": { "status": "interrupted", "result": { "persisted_locally": false } },
    });
    if request_json(
        &client,
        &config,
        Method::PATCH,
        &format!("/runs/{run_id}"),
        &body,
    )
    .await
    .is_err()
    {
        mark_degraded(&degraded, &emitter);
    }
}

async fn request_json(
    client: &reqwest::Client,
    config: &CloudConfig,
    method: Method,
    path: &str,
    body: &Value,
) -> Result<Value, ()> {
    let mut response = client
        .request(method, format!("{}{}", config.base_url, path))
        .bearer_auth(config.token.as_str())
        .json(body)
        .send()
        .await
        .map_err(|_| ())?;
    if !response.status().is_success()
        || response.content_length().is_some_and(|length| length > MAX_RESPONSE_BYTES)
    {
        return Err(());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES as usize {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| ())
}

fn mark_degraded(degraded: &AtomicBool, emitter: &EventEmitter) {
    if !degraded.swap(true, Ordering::AcqRel) {
        emitter.emit(
            "durability.updated",
            json!({
                "local": true,
                "cloud": false,
                "degraded": true,
                "reason": "Cloud record sync is unavailable; the local task continues.",
            }),
        );
    }
}

fn opaque_id(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

fn sanitized_operation(name: &str, result: &Result<String, String>) -> Value {
    let parsed = result.as_ref().ok().and_then(|output| parse_untrusted_json(output));
    let ok = parsed
        .as_ref()
        .and_then(|value| value.get("ok"))
        .and_then(Value::as_bool)
        .unwrap_or_else(|| result.is_ok());
    let channel = parsed
        .as_ref()
        .and_then(|value| value.get("channel"))
        .and_then(Value::as_str)
        .unwrap_or("bailey_local");
    let tool = parsed
        .as_ref()
        .and_then(|value| value.get("tool"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| name.rsplit("__").next().unwrap_or(name));
    let driver = parsed
        .as_ref()
        .and_then(|value| value.get("driver"))
        .map(sanitize_driver)
        .unwrap_or_else(|| json!({ "name": "bailey-local-tools" }));
    let safe_result = parsed
        .as_ref()
        .and_then(|value| value.get("result"))
        .map(sanitize_result)
        .unwrap_or(Value::Null);
    let error = if ok {
        Value::Null
    } else {
        let code = parsed
            .as_ref()
            .and_then(|value| value.pointer("/error/code"))
            .and_then(Value::as_str)
            .unwrap_or("TOOL_CALL_FAILED");
        json!({ "code": code })
    };
    json!({
        "type": "local.operation.record",
        "version": "0.1",
        "ok": ok,
        "channel": channel,
        "tool": tool,
        "driver": driver,
        "result": safe_result,
        "error": error,
    })
}

fn parse_untrusted_json(output: &str) -> Option<Value> {
    let start = output.find('\n')? + 1;
    let end = output.rfind("\n</untrusted>")?;
    serde_json::from_str(&output[start..end]).ok()
}

fn sanitize_driver(value: &Value) -> Value {
    let mut safe = serde_json::Map::new();
    for field in ["name", "version", "protocol"] {
        if let Some(value) = value.get(field).filter(|value| value.is_string()) {
            safe.insert(field.to_string(), value.clone());
        }
    }
    Value::Object(safe)
}

fn sanitize_result(value: &Value) -> Value {
    let mut safe = serde_json::Map::new();
    for field in ["sent", "verified", "status"] {
        if let Some(value) = value.get(field).filter(|value| {
            value.is_boolean() || value.is_number() || value.is_string()
        }) {
            safe.insert(field.to_string(), value.clone());
        }
    }
    if let Some(verification) = value.get("verification") {
        let count = verification
            .get("evidence")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        let mut summary = serde_json::Map::new();
        summary.insert("evidence_count".to_string(), json!(count));
        if let Some(hash) = verification.get("messageHash").and_then(Value::as_str) {
            summary.insert("message_hash".to_string(), json!(hash));
        }
        if let Some(observed) = verification.get("observedAt").and_then(Value::as_str) {
            summary.insert("observed_at".to_string(), json!(observed));
        }
        safe.insert("verification".to_string(), Value::Object(summary));
    }
    Value::Object(safe)
}

fn evidence_summary(operation: &Value) -> Option<Value> {
    let count = operation
        .pointer("/result/verification/evidence_count")
        .and_then(Value::as_u64)?;
    (count > 0).then(|| {
        json!({
            "tool": operation.get("tool").and_then(Value::as_str),
            "count": count,
            "content_uploaded": false,
            "paths_uploaded": false,
        })
    })
}
