use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, oneshot};

use crate::agent::engine::{AgentEngine, TurnCtx, TurnStop};
use crate::agent::jobs::{JobTable, SharedJobs};
use crate::agent::mcp::{FilteredTools, McpClient};
use crate::services::code_session_store::CodeSessionLease;
use crate::services::models_cache::ModelsCache;
use crate::services::session_store::{ApiKey, CodeSessionState, SessionStore, StoredChatMessage};

use super::protocol::{INTERNAL_ERROR, NOT_FOUND, RpcFailure, THREAD_BUSY, UNAVAILABLE};
use super::ui::{AppServerUi, EventEmitter, Outbound, PendingInteractions, fail_pending_for_turn};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicProvider {
    pub kind: String,
    pub label: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicMcpStatus {
    pub scope: &'static str,
    pub connected_servers: usize,
    pub tools: usize,
    pub issues: usize,
    pub degraded: bool,
}

pub fn public_provider_for_base_url(base_url: &str) -> PublicProvider {
    use crate::services::provider_profile::ProviderKind;

    if crate::services::provider_profile::is_aivo_starter_base(base_url) {
        return PublicProvider {
            kind: "aivo_starter".to_string(),
            label: "Aivo Starter".to_string(),
        };
    }
    let (kind, label) = match crate::services::provider_profile::provider_profile_for_base_url(
        base_url,
    )
    .kind
    {
        ProviderKind::Copilot => ("copilot", "GitHub Copilot"),
        ProviderKind::CursorAcp => ("cursor_acp", "Cursor"),
        ProviderKind::Ollama => ("ollama", "Ollama"),
        ProviderKind::OpenRouter => ("openrouter", "OpenRouter"),
        ProviderKind::CloudflareAi => ("cloudflare_ai", "Cloudflare AI"),
        ProviderKind::AnthropicCompatible => ("anthropic_compatible", "Anthropic-compatible"),
        ProviderKind::GoogleNative => ("google_native", "Google"),
        ProviderKind::OpenAiCompatible => ("openai_compatible", "OpenAI-compatible"),
    };
    PublicProvider {
        kind: kind.to_string(),
        label: label.to_string(),
    }
}

struct ActiveTurn {
    turn_id: String,
    task: tokio::task::JoinHandle<()>,
    terminal: Arc<AtomicBool>,
}

pub const NEW_THREAD_TITLE: &str = "新任务";

#[derive(Clone)]
struct DurableConversation {
    title: String,
    messages: Vec<StoredChatMessage>,
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

struct CancelledTurn {
    thread_id: String,
    turn_id: String,
    seq: Arc<AtomicU64>,
}

impl PreparedCancel {
    async fn abort(self, pending: &PendingInteractions) -> CancelledTurn {
        let Self {
            thread_id,
            turn_id,
            task,
            seq,
        } = self;
        task.abort();
        let _ = task.await;
        fail_pending_for_turn(pending, &thread_id, &turn_id);
        CancelledTurn {
            thread_id,
            turn_id,
            seq,
        }
    }

    pub async fn execute(
        self,
        runtime: &ThreadRuntime,
        outbound: &Outbound,
        pending: &PendingInteractions,
    ) {
        let cancelled = self.abort(pending).await;
        let persisted = runtime.persist_interrupted_engine().await.is_ok();
        cancelled.emit(outbound, persisted);
    }
}

impl CancelledTurn {
    fn emit(self, outbound: &Outbound, persisted: bool) {
        let emitter = EventEmitter::new(outbound.clone(), self.thread_id, self.turn_id, self.seq);
        if !persisted {
            emitter.emit(
                "error",
                json!({ "text": "Cancelled turn context could not be saved." }),
            );
        }
        emitter.emit(
            "turn.cancelled",
            json!({ "sideEffectsRolledBack": false, "persisted": persisted }),
        );
    }
}

impl PreparedTurn {
    pub fn start(self) {
        let _ = self.start.send(());
    }
}

pub struct ThreadRuntime {
    pub id: String,
    pub session_id: String,
    pub cwd: PathBuf,
    pub raw_model: String,
    provider: PublicProvider,
    mcp: PublicMcpStatus,
    credential_base_url: String,
    key: ApiKey,
    _lease: CodeSessionLease,
    store: SessionStore,
    engine: Arc<Mutex<AgentEngine>>,
    conversation: Arc<Mutex<DurableConversation>>,
    jobs: SharedJobs,
    active: Arc<Mutex<Option<ActiveTurn>>>,
    seq: Arc<AtomicU64>,
}

impl ThreadRuntime {
    pub fn provider(&self) -> &PublicProvider {
        &self.provider
    }

    pub fn mcp(&self) -> &PublicMcpStatus {
        &self.mcp
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        id: String,
        session_id: String,
        lease: CodeSessionLease,
        cwd: String,
        key_id: Option<String>,
        requested_model: Option<String>,
        store: SessionStore,
        cache: &ModelsCache,
    ) -> Result<Arc<Self>, RpcFailure> {
        Self::create_with_conversation(
            id,
            session_id,
            lease,
            cwd,
            key_id,
            requested_model,
            store,
            cache,
            DurableConversation {
                title: NEW_THREAD_TITLE.to_string(),
                messages: Vec::new(),
            },
            None,
        )
        .await
    }

    pub async fn resume(
        id: String,
        state: CodeSessionState,
        title: String,
        lease: CodeSessionLease,
        store: SessionStore,
        cache: &ModelsCache,
    ) -> Result<Arc<Self>, RpcFailure> {
        let CodeSessionState {
            session_id,
            key_id,
            cwd,
            model,
            messages,
            engine_messages,
            ..
        } = state;
        Self::create_with_conversation(
            id,
            session_id,
            lease,
            cwd,
            Some(key_id),
            Some(model),
            store,
            cache,
            DurableConversation { title, messages },
            engine_messages,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_with_conversation(
        id: String,
        session_id: String,
        lease: CodeSessionLease,
        cwd: String,
        key_id: Option<String>,
        requested_model: Option<String>,
        store: SessionStore,
        cache: &ModelsCache,
        conversation: DurableConversation,
        engine_messages: Option<Vec<Value>>,
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
        let provider = public_provider_for_base_url(&key.base_url);

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

        // App Server v1 has no project-MCP consent flow. Load only the user's
        // explicit global config so opening a repo can never execute its
        // `.mcp.json` stdio commands without approval. Connection/config errors
        // are best-effort: the thread remains usable with the tools that did
        // connect, and the public status exposes counts without leaking details.
        let disabled_mcp_servers = store.get_disabled_mcp_servers().await;
        let disabled_mcp_tools = store.get_disabled_mcp_tools().await;
        let (mcp, mcp_client, disabled_mcp_tools) =
            match (disabled_mcp_servers, disabled_mcp_tools) {
                (Ok(disabled_servers), Ok(disabled_tools)) => {
                    let disabled_servers = disabled_servers
                        .into_iter()
                        .collect::<std::collections::HashSet<_>>();
                    let disabled_tools = disabled_tools
                        .into_iter()
                        .collect::<std::collections::HashSet<_>>();
                    let client = Arc::new(
                        McpClient::connect_user_config_enabled(&disabled_servers).await,
                    );
                    let issues = client.errors().len();
                    let status = PublicMcpStatus {
                        scope: "user",
                        connected_servers: client.connected_server_count(),
                        tools: client.available_tool_count(&disabled_tools),
                        issues,
                        degraded: issues > 0,
                    };
                    (status, Some(client), disabled_tools)
                }
                (servers, tools) => {
                    let issues = servers.is_err() as usize + tools.is_err() as usize;
                    (
                        PublicMcpStatus {
                            scope: "user",
                            connected_servers: 0,
                            tools: 0,
                            issues,
                            degraded: true,
                        },
                        None,
                        std::collections::HashSet::new(),
                    )
                }
            };
        if let Some(mcp_client) = mcp_client
            && mcp_client.has_tools()
        {
            if disabled_mcp_tools.is_empty() {
                engine.set_external_tools(mcp_client);
            } else {
                engine.set_external_tools(Arc::new(FilteredTools::new(
                    mcp_client,
                    disabled_mcp_tools,
                )));
            }
        }
        if let Some(messages) = engine_messages {
            engine.restore_conversation(messages);
        } else {
            engine.seed_history(
                conversation
                    .messages
                    .iter()
                    .filter(|message| matches!(message.role.as_str(), "user" | "assistant"))
                    .map(|message| (message.role.clone(), message.content.clone())),
            );
        }
        let jobs = JobTable::new(Some(store.session_artifacts_dir(&session_id).join("jobs")));
        engine.set_jobs(jobs.clone());
        engine.set_artifacts_dir(store.session_artifacts_dir(&session_id));
        engine.maybe_enable_lsp(&cwd);
        store
            .record_selection(&key.id, "app-server", Some(&raw_model))
            .await
            .map_err(internal)?;

        Ok(Arc::new(Self {
            id,
            session_id,
            cwd,
            raw_model,
            provider,
            mcp,
            credential_base_url: context_base,
            key,
            _lease: lease,
            store,
            engine: Arc::new(Mutex::new(engine)),
            conversation: Arc::new(Mutex::new(conversation)),
            jobs,
            active: Arc::new(Mutex::new(None)),
            seq: Arc::new(AtomicU64::new(0)),
        }))
    }

    pub async fn title(&self) -> String {
        self.conversation.lock().await.title.clone()
    }

    pub async fn persist_empty(&self) -> Result<(), RpcFailure> {
        self.persist_snapshot(None).await
    }

    async fn persist_user_turn(&self, turn_id: &str, user_text: &str) -> Result<(), RpcFailure> {
        let now = chrono::Utc::now().to_rfc3339();
        let previous = {
            let mut conversation = self.conversation.lock().await;
            let previous = conversation.clone();
            let has_user = conversation
                .messages
                .iter()
                .any(|message| message.role == "user" && !message.content.trim().is_empty());
            if !has_user {
                conversation.title =
                    title_from_text(user_text).unwrap_or_else(|| NEW_THREAD_TITLE.to_string());
            }
            conversation.messages.push(StoredChatMessage {
                role: "user".to_string(),
                content: user_text.to_string(),
                reasoning_content: None,
                id: Some(format!("{turn_id}:user")),
                timestamp: Some(now),
                attachments: None,
                model: None,
            });
            previous
        };
        if let Err(error) = self.persist_snapshot(None).await {
            *self.conversation.lock().await = previous;
            return Err(error);
        }
        // The accepted user turn is now newer than the previously exact engine
        // transcript. Clear that blob until completion refreshes it so a crash or
        // cancellation resumes from the full display history rather than silently
        // dropping the acknowledged prompt.
        if let Err(error) = self
            .store
            .save_agent_messages(&self.session_id, &[])
            .await
            .map_err(internal)
        {
            *self.conversation.lock().await = previous;
            let _ = self.persist_snapshot(None).await;
            return Err(error);
        }
        Ok(())
    }

    async fn persist_turn_completion(
        &self,
        turn_id: &str,
        assistant_text: String,
        engine_messages: &[Value],
    ) -> Result<(), RpcFailure> {
        if !assistant_text.trim().is_empty() {
            let now = chrono::Utc::now().to_rfc3339();
            let mut conversation = self.conversation.lock().await;
            conversation.messages.push(StoredChatMessage {
                role: "assistant".to_string(),
                content: assistant_text,
                reasoning_content: None,
                id: Some(format!("{turn_id}:assistant")),
                timestamp: Some(now),
                attachments: None,
                model: Some(self.raw_model.clone()),
            });
        }
        self.persist_snapshot(Some(engine_messages)).await
    }

    async fn persist_interrupted_engine(&self) -> Result<(), RpcFailure> {
        let messages = self.recoverable_engine_messages().await;
        self.store
            .save_agent_messages(&self.session_id, &messages)
            .await
            .map_err(internal)
    }

    async fn recoverable_engine_messages(&self) -> Vec<Value> {
        let latest_user = self
            .conversation
            .lock()
            .await
            .messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map(|message| message.content.clone());
        let conversation = self.engine.lock().await.export_conversation();
        let covers_latest_user = latest_user.as_deref().is_some_and(|latest| {
            conversation
                .iter()
                .rev()
                .find(|message| message["role"] == "user")
                .and_then(|message| message["content"].as_str())
                == Some(latest)
        });
        // Cancellation can arrive while the route is still starting, before the
        // engine has appended this turn's user message. Keep the exact blob clear
        // in that case so resume seeds the already-durable display history.
        if covers_latest_user {
            conversation
        } else {
            Vec::new()
        }
    }

    pub async fn flush(&self) -> Result<(), RpcFailure> {
        if let Some(active) = self.active.lock().await.as_ref() {
            return Err(RpcFailure::new(THREAD_BUSY, "thread has an active turn")
                .with_data(json!({ "threadId": self.id, "turnId": active.turn_id })));
        }
        let messages = self.recoverable_engine_messages().await;
        self.persist_snapshot(Some(&messages)).await
    }

    async fn persist_snapshot(&self, engine_messages: Option<&[Value]>) -> Result<(), RpcFailure> {
        let conversation = self.conversation.lock().await.clone();
        let preview = conversation_preview(&conversation.messages);
        let tokens = self.store.chat_session_tokens(&self.session_id).await;
        self.store
            .save_code_session_with_id(
                &self.key.id,
                &self.credential_base_url,
                &self.cwd.to_string_lossy(),
                &self.session_id,
                &self.raw_model,
                None,
                &conversation.messages,
                &conversation.title,
                &preview,
                tokens,
            )
            .await
            .map_err(internal)?;
        if let Some(messages) = engine_messages {
            self.store
                .save_agent_messages(&self.session_id, messages)
                .await
                .map_err(internal)?;
        }
        Ok(())
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

        self.persist_user_turn(&turn_id, &text).await?;

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
        let conversation = engine.export_conversation();
        drop(engine);

        let assistant_text = ui.answer().to_string();
        let persist_error = self
            .persist_turn_completion(&turn_id, assistant_text.clone(), &conversation)
            .await
            .err();

        if persist_error.is_some() {
            emitter.emit(
                "error",
                json!({
                    "text": "Conversation could not be saved; this turn remains available only while the runtime is open."
                }),
            );
        }
        let persisted = persist_error.is_none();
        let (event_type, mut payload) = if let Some(error) = ui.last_error.as_deref() {
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
        payload["persisted"] = Value::Bool(persisted);
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
            cancel.execute(self, outbound, pending).await;
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

pub(super) fn canonicalize_cwd(cwd: &str) -> Result<PathBuf, RpcFailure> {
    validate_cwd(cwd)
}

pub(super) fn title_from_messages(messages: &[StoredChatMessage], model: &str) -> String {
    messages
        .iter()
        .find(|message| message.role == "user" && !message.content.trim().is_empty())
        .and_then(|message| title_from_text(&message.content))
        .unwrap_or_else(|| {
            if messages.is_empty() {
                NEW_THREAD_TITLE.to_string()
            } else {
                model.to_string()
            }
        })
}

fn conversation_preview(messages: &[StoredChatMessage]) -> String {
    let snippets = messages
        .iter()
        .rev()
        .filter(|message| matches!(message.role.as_str(), "user" | "assistant"))
        .filter(|message| !message.content.trim().is_empty())
        .take(2)
        .map(|message| {
            message
                .content
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>();
    snippets.into_iter().rev().collect::<Vec<_>>().join(" · ")
}

fn first_non_empty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

fn title_from_text(text: &str) -> Option<String> {
    let line = first_non_empty_line(text)?;
    let mut chars = line.chars();
    let mut title = chars.by_ref().take(34).collect::<String>();
    if chars.next().is_some() {
        title.push('…');
    }
    Some(title)
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

    #[test]
    fn title_uses_first_line_and_truncates_by_unicode_scalar() {
        assert_eq!(
            title_from_text("\n  第一行标题  \n第二行"),
            Some("第一行标题".to_string())
        );
        let long = "界".repeat(35);
        assert_eq!(
            title_from_text(&long),
            Some(format!("{}…", "界".repeat(34)))
        );
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
        let cancelled = cancel.abort(&pending).await;
        cancelled.emit(&outbound, true);

        let in_flight = messages.recv().await.unwrap();
        let terminal = messages.recv().await.unwrap();
        assert_eq!(in_flight["params"]["type"], "notice");
        assert_eq!(in_flight["params"]["seq"], 1);
        assert_eq!(terminal["params"]["type"], "turn.cancelled");
        assert_eq!(terminal["params"]["seq"], 2);
        assert!(messages.try_recv().is_err());
    }
}
