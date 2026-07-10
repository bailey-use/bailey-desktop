use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::sync::{Mutex, oneshot};

use crate::agent::engine::{AgentEngine, TurnCtx, TurnStop};
use crate::agent::jobs::{JobTable, SharedJobs};
use crate::services::models_cache::ModelsCache;
use crate::services::session_store::{ApiKey, SessionStore};

use super::protocol::{INTERNAL_ERROR, NOT_FOUND, RpcFailure, THREAD_BUSY, UNAVAILABLE};
use super::ui::{AppServerUi, EventEmitter, Outbound, PendingInteractions, fail_pending_for_turn};

struct ActiveTurn {
    turn_id: String,
    task: tokio::task::JoinHandle<()>,
    terminal: Arc<AtomicBool>,
}

pub struct PreparedTurn {
    pub turn_id: String,
    start: oneshot::Sender<()>,
}

pub struct PreparedCancel {
    thread_id: String,
    turn_id: String,
    task: tokio::task::JoinHandle<()>,
    seq: Arc<AtomicU64>,
}

impl PreparedCancel {
    pub async fn execute(self, outbound: &Outbound, pending: &PendingInteractions) {
        let Self {
            thread_id,
            turn_id,
            task,
            seq,
        } = self;
        task.abort();
        let _ = task.await;
        fail_pending_for_turn(pending, &thread_id, &turn_id);
        let emitter = EventEmitter::new(outbound.clone(), thread_id, turn_id, seq);
        emitter.emit("turn.cancelled", json!({ "sideEffectsRolledBack": false }));
    }
}

impl PreparedTurn {
    pub fn start(self) {
        let _ = self.start.send(());
    }
}

pub struct ThreadRuntime {
    pub id: String,
    pub cwd: PathBuf,
    pub raw_model: String,
    pub key_name: String,
    key: ApiKey,
    store: SessionStore,
    engine: Arc<Mutex<AgentEngine>>,
    jobs: SharedJobs,
    active: Arc<Mutex<Option<ActiveTurn>>>,
    seq: Arc<AtomicU64>,
}

impl ThreadRuntime {
    pub async fn create(
        id: String,
        cwd: String,
        key_id: Option<String>,
        requested_model: Option<String>,
        store: SessionStore,
        cache: &ModelsCache,
    ) -> Result<Arc<Self>, RpcFailure> {
        let cwd = validate_cwd(&cwd)?;
        let mut key = match key_id.as_deref() {
            Some(id) => store
                .resolve_key_by_id_or_name(id)
                .await
                .map_err(|e| RpcFailure::new(NOT_FOUND, format!("API key not found: {e}")))?,
            None => store
                .get_active_key()
                .await
                .map_err(internal)?
                .ok_or_else(|| RpcFailure::new(UNAVAILABLE, "no active API key"))?,
        };
        if key.is_any_oauth() || key.is_cursor_acp() {
            return Err(RpcFailure::new(
                UNAVAILABLE,
                "the selected key cannot drive the in-process AgentEngine",
            ));
        }

        let raw_model = match requested_model.filter(|model| !model.trim().is_empty()) {
            Some(model) => model,
            None => store
                .get_code_model(&key.id)
                .await
                .map_err(internal)?
                .ok_or_else(|| {
                    RpcFailure::new(
                        UNAVAILABLE,
                        "no coding model selected for the active key; pass `model` to thread/start",
                    )
                })?,
        };
        let context_base = key.base_url.clone();
        let context_window =
            crate::services::model_metadata::resolve_limits(cache, Some(&context_base), &raw_model)
                .await
                .context
                .unwrap_or(0)
                .min(u32::MAX as u64) as u32;
        let model = crate::services::model_names::transform_model_for_provider(
            None,
            &key.base_url,
            &raw_model,
        );

        if key.base_url == "ollama" {
            crate::services::ollama::ensure_ready()
                .await
                .map_err(internal)?;
            crate::services::ollama::ensure_model(&raw_model)
                .await
                .map_err(internal)?;
            key.base_url = crate::services::ollama::ollama_openai_base_url();
        }

        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let guides = crate::agent::system_prompt::discover_project_guides(&cwd);
        let mut skills = crate::agent::skills::discover_skills(&cwd);
        if let Ok(disabled) = store.get_disabled_skills().await {
            let disabled = disabled
                .into_iter()
                .collect::<std::collections::HashSet<_>>();
            skills.retain(|skill| !disabled.contains(&skill.name));
        }
        let mut engine = AgentEngine::new(
            &cwd.to_string_lossy(),
            &model,
            &date,
            &guides,
            &skills,
            context_window,
            0,
        );
        if crate::services::provider_profile::is_aivo_starter_base(&context_base) {
            engine.set_first_party();
        }
        engine.set_subagents(&crate::agent::subagents::discover_subagents(
            store.config_dir(),
        ));
        engine.set_grants_path(store.config_dir());
        engine.enable_rewind_checkpoints(cwd.to_string_lossy().as_ref());
        engine.set_confirm_before_build();
        engine.enable_user_input();
        let jobs = JobTable::new(Some(store.session_artifacts_dir(&id).join("jobs")));
        engine.set_jobs(jobs.clone());
        engine.set_artifacts_dir(store.session_artifacts_dir(&id));
        engine.maybe_enable_lsp(&cwd);
        store
            .record_selection(&key.id, "app-server", Some(&raw_model))
            .await
            .map_err(internal)?;

        Ok(Arc::new(Self {
            id,
            cwd,
            raw_model,
            key_name: key.display_name().to_string(),
            key,
            store,
            engine: Arc::new(Mutex::new(engine)),
            jobs,
            active: Arc::new(Mutex::new(None)),
            seq: Arc::new(AtomicU64::new(0)),
        }))
    }

    pub async fn prepare_turn(
        self: &Arc<Self>,
        turn_id: String,
        text: String,
        outbound: Outbound,
        pending: PendingInteractions,
        request_seq: Arc<AtomicU64>,
    ) -> Result<PreparedTurn, RpcFailure> {
        if text.trim().is_empty() {
            return Err(RpcFailure::new(
                super::protocol::INVALID_PARAMS,
                "turn text cannot be empty",
            ));
        }
        let mut active = self.active.lock().await;
        if let Some(active) = active.as_ref() {
            return Err(
                RpcFailure::new(THREAD_BUSY, "thread already has an active turn")
                    .with_data(json!({ "threadId": self.id, "turnId": active.turn_id })),
            );
        }

        let terminal = Arc::new(AtomicBool::new(false));
        let emitter = EventEmitter::new(
            outbound.clone(),
            self.id.clone(),
            turn_id.clone(),
            self.seq.clone(),
        );
        let (start, start_rx) = oneshot::channel();
        let runtime = self.clone();
        let task_terminal = terminal.clone();
        let task_turn_id = turn_id.clone();
        let task_active = self.active.clone();
        let handle = tokio::spawn(async move {
            if start_rx.await.is_err() {
                return;
            }
            runtime
                .run_turn(
                    task_turn_id.clone(),
                    text,
                    emitter,
                    outbound,
                    pending,
                    request_seq,
                    task_terminal,
                )
                .await;
            let mut active = task_active.lock().await;
            if active.as_ref().map(|turn| turn.turn_id.as_str()) == Some(task_turn_id.as_str()) {
                *active = None;
            }
        });
        *active = Some(ActiveTurn {
            turn_id: turn_id.clone(),
            task: handle,
            terminal,
        });
        Ok(PreparedTurn { turn_id, start })
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_turn(
        &self,
        turn_id: String,
        text: String,
        emitter: EventEmitter,
        outbound: Outbound,
        pending: PendingInteractions,
        request_seq: Arc<AtomicU64>,
        terminal: Arc<AtomicBool>,
    ) {
        emitter.emit(
            "turn.started",
            json!({ "model": self.raw_model, "cwd": self.cwd }),
        );
        let route = match TurnRoute::start(&self.key, &self.store).await {
            Ok(route) => route,
            Err(error) => {
                emit_terminal_once(
                    &terminal,
                    &emitter,
                    "turn.failed",
                    json!({ "error": redact(&format!("agent route failed: {error:#}")) }),
                );
                return;
            }
        };
        let client = crate::services::http_utils::router_http_client_loopback();
        let ctx = TurnCtx {
            client: &client,
            serve_base: &route.base,
            auth: route.auth.as_deref(),
            cwd: &self.cwd,
            yes: false,
            auto_approve_all: false,
            auto_approve: None,
            review_edits: None,
        };
        let pending_for_cleanup = pending.clone();
        let mut ui = AppServerUi::new(emitter.clone(), outbound, pending, request_seq);
        let mut engine = self.engine.lock().await;
        engine.run_turn(&ctx, &mut ui, text).await;
        ui.finish_streams();
        let _conversation = engine.export_conversation();
        drop(engine);

        let (event_type, payload) = if let Some(error) = ui.last_error.as_deref() {
            (
                "turn.failed",
                json!({ "error": redact(error), "text": redact(ui.answer()) }),
            )
        } else if let Some(stop) = ui.stopped {
            (
                "turn.stopped",
                json!({ "reason": stop_name(stop), "text": redact(ui.answer()) }),
            )
        } else {
            ("turn.completed", json!({ "text": redact(ui.answer()) }))
        };
        emit_terminal_once(&terminal, &emitter, event_type, payload);
        fail_pending_for_turn(&pending_for_cleanup, &self.id, &turn_id);
    }

    pub async fn prepare_cancel(
        &self,
        turn_id: &str,
    ) -> Result<Option<PreparedCancel>, RpcFailure> {
        let mut active = self.active.lock().await;
        let Some(current) = active.as_ref() else {
            return Ok(None);
        };
        if current.turn_id != turn_id {
            return Err(RpcFailure::new(NOT_FOUND, "active turn id does not match")
                .with_data(json!({ "activeTurnId": current.turn_id })));
        }
        if current
            .terminal
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(None);
        }
        let current = active.take().unwrap();
        Ok(Some(PreparedCancel {
            thread_id: self.id.clone(),
            turn_id: turn_id.to_string(),
            task: current.task,
            seq: self.seq.clone(),
        }))
    }

    pub async fn shutdown(&self, outbound: &Outbound, pending: &PendingInteractions) {
        let turn_id = self
            .active
            .lock()
            .await
            .as_ref()
            .map(|turn| turn.turn_id.clone());
        if let Some(turn_id) = turn_id
            && let Ok(Some(cancel)) = self.prepare_cancel(&turn_id).await
        {
            cancel.execute(outbound, pending).await;
        }
        let _ = self.jobs.kill_all().await;
    }
}

struct TurnRoute {
    base: String,
    auth: Option<String>,
    cleanup: Option<RouterCleanup>,
}

impl TurnRoute {
    async fn start(key: &ApiKey, store: &SessionStore) -> anyhow::Result<Self> {
        if let Ok(script) = std::env::var("AIVO_AGENT_FAKE_SSE") {
            let bodies = crate::services::fake_model::load_script(&script)
                .map_err(|error| anyhow::anyhow!(error))?;
            let port = crate::services::fake_model::start(bodies)?;
            return Ok(Self {
                base: format!("http://127.0.0.1:{port}"),
                auth: None,
                cleanup: None,
            });
        }

        use crate::services::serve_router::{ServeRouter, ServeRouterConfig, random_auth_token};
        let auth = random_auth_token();
        let config = ServeRouterConfig::from_key(
            key,
            false,
            300,
            Some(auth.clone()),
            std::collections::HashMap::new(),
        );
        let router = ServeRouter::new(config, key.clone(), store.logs())
            .with_usage_accounting(store.clone(), "app-server".to_string())
            .quiet(true);
        let (handle, shutdown, port) = router.start_background_with_addr("127.0.0.1", 0).await?;
        Ok(Self {
            base: format!("http://127.0.0.1:{port}"),
            auth: Some(auth),
            cleanup: Some(RouterCleanup { handle, shutdown }),
        })
    }
}

impl Drop for TurnRoute {
    fn drop(&mut self) {
        let _ = self.cleanup.take();
    }
}

struct RouterCleanup {
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    shutdown: Arc<tokio::sync::Notify>,
}

impl Drop for RouterCleanup {
    fn drop(&mut self) {
        self.shutdown.notify_one();
        self.handle.abort();
    }
}

fn validate_cwd(cwd: &str) -> Result<PathBuf, RpcFailure> {
    let cwd = Path::new(cwd);
    if !cwd.is_dir() {
        return Err(RpcFailure::new(
            super::protocol::INVALID_PARAMS,
            "cwd must be an existing directory",
        ));
    }
    std::fs::canonicalize(cwd).map_err(internal)
}

fn internal(error: impl std::fmt::Display) -> RpcFailure {
    RpcFailure::new(INTERNAL_ERROR, error.to_string())
}

fn emit_terminal_once(
    terminal: &AtomicBool,
    emitter: &EventEmitter,
    event_type: &str,
    payload: Value,
) {
    if !terminal.swap(true, Ordering::AcqRel) {
        emitter.emit(event_type, payload);
    }
}

fn stop_name(stop: TurnStop) -> &'static str {
    match stop {
        TurnStop::NoProgress => "no_progress",
        TurnStop::ToolFailureLoop => "tool_failure_loop",
        TurnStop::StepLimit => "step_limit",
    }
}

fn redact(text: &str) -> String {
    crate::agent::secrets_guard::redact_for_model(text).0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    struct EmitOnDrop(Option<EventEmitter>);

    impl Drop for EmitOnDrop {
        fn drop(&mut self) {
            if let Some(emitter) = self.0.take() {
                emitter.emit("notice", json!({ "text": "in flight" }));
            }
        }
    }

    #[tokio::test]
    async fn cancellation_waits_for_in_flight_events_before_the_terminal_event() {
        let (outbound, mut messages) = mpsc::unbounded_channel();
        let seq = Arc::new(AtomicU64::new(0));
        let emitter = EventEmitter::new(
            outbound.clone(),
            "thread_1".to_string(),
            "turn_1".to_string(),
            seq.clone(),
        );
        let task = tokio::spawn(async move {
            let _emit_on_drop = EmitOnDrop(Some(emitter));
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        let cancel = PreparedCancel {
            thread_id: "thread_1".to_string(),
            turn_id: "turn_1".to_string(),
            task,
            seq,
        };
        let pending = PendingInteractions::default();
        cancel.execute(&outbound, &pending).await;

        let in_flight = messages.recv().await.unwrap();
        let terminal = messages.recv().await.unwrap();
        assert_eq!(in_flight["params"]["type"], "notice");
        assert_eq!(in_flight["params"]["seq"], 1);
        assert_eq!(terminal["params"]["type"], "turn.cancelled");
        assert_eq!(terminal["params"]["seq"], 2);
        assert!(messages.try_recv().is_err());
    }
}
