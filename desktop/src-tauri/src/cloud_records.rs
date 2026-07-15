use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::Method;
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter};
use tokio::sync::{mpsc, watch};
use zeroize::Zeroizing;

const RECORD_QUEUE_CAPACITY: usize = 256;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const RECORD_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const TERMINAL_RETRY_ATTEMPTS: usize = 2;
const TERMINAL_RETRY_DELAY: Duration = Duration::from_millis(200);
const RELAY_CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);
const RELAY_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
const RELAY_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(12);
const RELAY_ABORT_JOIN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub(crate) struct CloudRecordRelay {
    inner: Arc<RelayInner>,
}

struct RelayInner {
    tx: mpsc::Sender<ObservedEvent>,
    app: AppHandle,
    cancelled: AtomicBool,
    cancel: watch::Sender<bool>,
    completed: watch::Receiver<bool>,
    worker: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

impl RelayInner {
    fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            let _ = self.cancel.send(true);
        }
    }
}

impl Drop for RelayInner {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = self.cancel.send(true);
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            // Explicit shutdown gets a bounded flush. If an owner forgets to
            // call it, abort instead of leaving the records key in a detached
            // task after the runtime has gone away.
            worker.abort();
        }
    }
}

struct WorkerCompletion(watch::Sender<bool>);

impl Drop for WorkerCompletion {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

#[derive(Clone)]
struct ObservedEvent {
    thread_id: String,
    turn_id: String,
    seq: u64,
    created_at: String,
    kind: ObservedEventKind,
}

#[derive(Clone)]
enum ObservedEventKind {
    Started { data: Value },
    Audit { event_type: String, data: Value },
    Terminal { status: &'static str, persisted: bool },
}

#[derive(Clone, Copy)]
struct TerminalState {
    status: &'static str,
    persisted: bool,
}

struct ActiveRun {
    run_id: String,
    terminal: Option<TerminalState>,
}

impl CloudRecordRelay {
    pub(crate) fn start(
        app: AppHandle,
        credential: &crate::account::RecordsCredential,
    ) -> Result<Self, String> {
        let client = crate::account::cloud_client()?;
        let base_url = credential.base_url.trim_end_matches('/').to_string();
        let token = Zeroizing::new(credential.api_key.clone());
        let (tx, rx) = mpsc::channel(RECORD_QUEUE_CAPACITY);
        let (cancel, cancel_rx) = watch::channel(false);
        let (completed_tx, completed) = watch::channel(false);
        let worker_app = app.clone();
        let completion = WorkerCompletion(completed_tx);
        let worker = tauri::async_runtime::spawn(async move {
            // Construct the guard before spawning so aborting an unpolled
            // future still publishes completion when its captures are dropped.
            let _completion = completion;
            run_worker(worker_app, client, base_url, token, rx, cancel_rx).await;
        });
        Ok(Self {
            inner: Arc::new(RelayInner {
                tx,
                app,
                cancelled: AtomicBool::new(false),
                cancel,
                completed,
                worker: Mutex::new(Some(worker)),
            }),
        })
    }

    pub(crate) fn observe_line(&self, line: &str) {
        if self.inner.cancelled.load(Ordering::Acquire) {
            return;
        }
        let Some(event) = parse_event(line) else {
            return;
        };
        if let Err(error) = self.inner.tx.try_send(event) {
            if !self.inner.cancelled.load(Ordering::Acquire) {
                emit_degraded(&self.inner.app, &error.into_inner());
            }
        }
    }

    pub(crate) fn cancel(&self) {
        self.inner.cancel();
    }

    /// Stop accepting observations, flush known active runs within a fixed
    /// deadline, then join the shared worker. Every clone targets the same
    /// cancellation state and completion signal; repeated calls are safe.
    pub(crate) async fn shutdown(&self) {
        self.cancel();

        let mut completed = self.inner.completed.clone();
        if *completed.borrow() {
            return;
        }

        let worker = self
            .inner
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(mut worker) = worker {
            if tokio::time::timeout(RELAY_SHUTDOWN_TIMEOUT, &mut worker)
                .await
                .is_err()
            {
                worker.abort();
                let _ = tokio::time::timeout(RELAY_ABORT_JOIN_TIMEOUT, &mut worker).await;
            }
            return;
        }

        // Another clone owns the join handle. Wait on the shared completion
        // signal, but never let a concurrent/repeated shutdown wait forever.
        let _ = tokio::time::timeout(RELAY_SHUTDOWN_TIMEOUT, async {
            while !*completed.borrow() {
                if completed.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
    }
}

async fn run_worker(
    app: AppHandle,
    client: reqwest::Client,
    base_url: String,
    token: Zeroizing<String>,
    mut rx: mpsc::Receiver<ObservedEvent>,
    cancel: watch::Receiver<bool>,
) {
    let mut active: HashMap<(String, String), ActiveRun> = HashMap::new();
    let mut degraded: HashSet<(String, String)> = HashSet::new();

    loop {
        if *cancel.borrow() {
            break;
        }
        let event = match tokio::time::timeout(RELAY_CANCEL_POLL_INTERVAL, rx.recv()).await {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(_) => continue,
        };
        let key = (event.thread_id.clone(), event.turn_id.clone());
        let failed = match &event.kind {
            ObservedEventKind::Started { data } => {
                if active.contains_key(&key) || degraded.contains(&key) {
                    continue;
                }
                match create_run(&client, &base_url, token.as_str()).await {
                    Ok(run_id) => {
                        active.insert(
                            key.clone(),
                            ActiveRun {
                                run_id: run_id.clone(),
                                terminal: None,
                            },
                        );
                        append_event(
                            &client,
                            &base_url,
                            token.as_str(),
                            &run_id,
                            "turn.started",
                            data.clone(),
                        )
                        .await
                        .is_err()
                    }
                    Err(()) => true,
                }
            }
            ObservedEventKind::Audit { event_type, data } => {
                if degraded.contains(&key) {
                    continue;
                }
                let Some(run) = active.get(&key) else {
                    continue;
                };
                append_event(
                    &client,
                    &base_url,
                    token.as_str(),
                    &run.run_id,
                    event_type,
                    data.clone(),
                )
                .await
                .is_err()
            }
            ObservedEventKind::Terminal { status, persisted } => {
                let terminal = TerminalState {
                    status: *status,
                    persisted: *persisted,
                };
                let Some(run) = active.get_mut(&key) else {
                    degraded.remove(&key);
                    continue;
                };
                run.terminal = Some(terminal);
                let run_id = run.run_id.clone();
                if finish_run_bounded(
                    &client,
                    &base_url,
                    token.as_str(),
                    &run_id,
                    *status,
                    *persisted,
                )
                .await
                .is_ok()
                {
                    active.remove(&key);
                    degraded.remove(&key);
                    false
                } else {
                    true
                }
            }
        };

        if failed {
            // A successfully created run always remains addressable, even if
            // an audit or terminal request fails. A later terminal event (or
            // shutdown flush) can therefore still close the Cloud record.
            degraded.insert(key);
            emit_degraded(&app, &event);
        }
    }

    let _ = tokio::time::timeout(
        RELAY_FLUSH_TIMEOUT,
        flush_active(&client, &base_url, token.as_str(), &mut active),
    )
    .await;

    for ((thread_id, turn_id), run) in active {
        let terminal = run.terminal.unwrap_or(TerminalState {
            status: "interrupted",
            persisted: false,
        });
        let event = ObservedEvent {
            thread_id,
            turn_id,
            seq: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            kind: ObservedEventKind::Terminal {
                status: terminal.status,
                persisted: terminal.persisted,
            },
        };
        emit_degraded(&app, &event);
    }
}

async fn flush_active(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    active: &mut HashMap<(String, String), ActiveRun>,
) {
    let keys = active.keys().cloned().collect::<Vec<_>>();
    for key in keys {
        let Some(run) = active.get(&key) else {
            continue;
        };
        let terminal = run.terminal.unwrap_or(TerminalState {
            status: "interrupted",
            persisted: false,
        });
        let run_id = run.run_id.clone();
        if finish_run_bounded(
            client,
            base_url,
            token,
            &run_id,
            terminal.status,
            terminal.persisted,
        )
        .await
        .is_ok()
        {
            active.remove(&key);
        }
    }
}

async fn create_run(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> Result<String, ()> {
    let body = json!({
        "goal": "Desktop agent turn",
        "status": "running",
        "context": { "runtime": "bailey-desktop" },
        "metadata": {
            "schema": "bailey.desktop.lifecycle/0.1",
        },
        "source": "bailey-desktop",
    });
    let response = request_json(client, base_url, token, Method::POST, "/runs", &body).await?;
    response
        .pointer("/run/run_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .map(str::to_string)
        .ok_or(())
}

async fn append_event(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    run_id: &str,
    event_type: &str,
    data: Value,
) -> Result<(), ()> {
    let body = json!({
        "event_type": event_type,
        "source": "bailey-desktop",
        "data": data,
    });
    request_json(
        client,
        base_url,
        token,
        Method::POST,
        &format!("/runs/{run_id}/events"),
        &body,
    )
    .await
    .map(|_| ())
}

async fn finish_run(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    run_id: &str,
    status: &str,
    persisted: bool,
) -> Result<(), ()> {
    let body = json!({
        "source": "bailey-desktop",
        "patch": {
            "status": status,
            "result": { "persisted_locally": persisted },
        },
    });
    request_json(
        client,
        base_url,
        token,
        Method::PATCH,
        &format!("/runs/{run_id}"),
        &body,
    )
    .await
    .map(|_| ())
}

async fn finish_run_bounded(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    run_id: &str,
    status: &str,
    persisted: bool,
) -> Result<(), ()> {
    for attempt in 0..TERMINAL_RETRY_ATTEMPTS {
        if finish_run(client, base_url, token, run_id, status, persisted)
            .await
            .is_ok()
        {
            return Ok(());
        }
        if attempt + 1 < TERMINAL_RETRY_ATTEMPTS {
            tokio::time::sleep(TERMINAL_RETRY_DELAY).await;
        }
    }
    Err(())
}

async fn request_json(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    method: Method,
    path: &str,
    body: &Value,
) -> Result<Value, ()> {
    tokio::time::timeout(RECORD_REQUEST_TIMEOUT, async {
        let mut response = client
            .request(method, format!("{base_url}{path}"))
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .map_err(|_| ())?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| ())
    })
    .await
    .map_err(|_| ())?
}

fn parse_event(line: &str) -> Option<ObservedEvent> {
    let message: Value = serde_json::from_str(line).ok()?;
    if message.get("method").and_then(Value::as_str) != Some("event") {
        return None;
    }
    let params = message.get("params")?;
    let thread_id = bounded_identifier(params.get("threadId")?.as_str()?)?;
    let turn_id = bounded_identifier(params.get("turnId")?.as_str()?)?;
    let seq = params.get("seq").and_then(Value::as_u64).unwrap_or(0);
    let created_at = params
        .get("createdAt")
        .and_then(Value::as_str)
        .filter(|value| value.len() <= 64 && !value.chars().any(char::is_control))
        .map(str::to_string)
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    let event_type = params.get("type")?.as_str()?;
    let payload = params.get("payload").cloned().unwrap_or_else(|| json!({}));
    let kind = match event_type {
        "turn.started" => ObservedEventKind::Started {
            data: json!({
                "local_persistence": true,
            }),
        },
        "tool.started" => ObservedEventKind::Audit {
            event_type: "tool.started".to_string(),
            data: json!({
                "tool_call_id": bounded_string(payload.get("toolCallId"), 128),
                "tool": bounded_string(payload.get("name"), 256),
            }),
        },
        "tool.completed" => ObservedEventKind::Audit {
            event_type: "tool.completed".to_string(),
            data: json!({
                "tool_call_id": bounded_string(payload.get("toolCallId"), 128),
                "tool": bounded_string(payload.get("name"), 256),
                "ok": payload.get("ok").and_then(Value::as_bool),
            }),
        },
        "turn.completed" => ObservedEventKind::Terminal {
            status: "completed",
            persisted: payload
                .get("persisted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "turn.stopped" => ObservedEventKind::Terminal {
            status: "stopped",
            persisted: payload
                .get("persisted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "turn.failed" => ObservedEventKind::Terminal {
            status: "failed",
            persisted: payload
                .get("persisted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "turn.cancelled" => ObservedEventKind::Terminal {
            status: "interrupted",
            persisted: payload
                .get("persisted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        _ => return None,
    };
    Some(ObservedEvent {
        thread_id,
        turn_id,
        seq,
        created_at,
        kind,
    })
}

fn bounded_identifier(value: &str) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':')))
    .then(|| value.to_string())
}

fn bounded_string(value: Option<&Value>, limit: usize) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|value| value.len() <= limit && !value.chars().any(char::is_control))
}

fn emit_degraded(app: &AppHandle, event: &ObservedEvent) {
    let message = json!({
        "jsonrpc": "2.0",
        "method": "event",
        "params": {
            "schemaVersion": 1,
            "seq": event.seq,
            "threadId": event.thread_id,
            "turnId": event.turn_id,
            "type": "durability.updated",
            "createdAt": event.created_at,
            "payload": {
                "local": true,
                "cloud": false,
                "degraded": true,
                "reason": "Cloud record sync is unavailable; the local task continues."
            }
        }
    });
    let _ = app.emit("app-server://message", message.to_string());
}
