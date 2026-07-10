mod protocol;
mod session;
mod ui;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::errors::ExitCode;
use crate::services::session_store::StoredChatMessage;
use crate::services::{ModelsCache, SessionStore};

use protocol::{
    IncomingMessage, InitializeParams, ModelListParams, NOT_INITIALIZED, RpcFailure,
    ThreadCloseParams, ThreadDeleteParams, ThreadFlushParams, ThreadListParams,
    ThreadResumeParams, ThreadStartParams, TurnCancelParams, TurnStartParams, UNSUPPORTED_VERSION,
};
use session::ThreadRuntime;
use ui::{Outbound, PendingInteractions};

const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
enum FrameRead {
    Ready,
    TooLarge,
}

pub async fn run_stdio(store: SessionStore, cache: ModelsCache) -> ExitCode {
    let (outbound, rx) = mpsc::unbounded_channel();
    let writer = tokio::spawn(write_stdout(rx));
    let mut server = AppServer::new(store, cache, outbound);
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut frame = Vec::new();

    loop {
        match read_frame(&mut stdin, &mut frame).await {
            Ok(Some(FrameRead::Ready)) => match std::str::from_utf8(&frame) {
                Ok(line) => {
                    if !server.handle_line(line).await {
                        break;
                    }
                }
                Err(error) => server.send_error(
                    Some(Value::Null),
                    RpcFailure::new(
                        protocol::INVALID_REQUEST,
                        format!("invalid UTF-8 JSON-RPC frame: {error}"),
                    ),
                ),
            },
            Ok(Some(FrameRead::TooLarge)) => server.send_error(
                Some(Value::Null),
                RpcFailure::new(protocol::INVALID_REQUEST, "JSON-RPC frame is too large"),
            ),
            Ok(None) => break,
            Err(error) => {
                eprintln!("aivo app-server: stdin error: {error}");
                break;
            }
        }
    }

    server.shutdown().await;
    drop(server);
    match writer.await {
        Ok(Ok(())) => ExitCode::Success,
        Ok(Err(error)) => {
            eprintln!("aivo app-server: stdout error: {error}");
            ExitCode::ToolExit(1)
        }
        Err(error) => {
            eprintln!("aivo app-server: writer task failed: {error}");
            ExitCode::ToolExit(1)
        }
    }
}

async fn read_frame<R>(reader: &mut R, buffer: &mut Vec<u8>) -> std::io::Result<Option<FrameRead>>
where
    R: AsyncBufRead + Unpin,
{
    buffer.clear();
    let mut oversized = false;

    loop {
        let (consumed, has_newline) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                if oversized {
                    return Ok(Some(FrameRead::TooLarge));
                }
                if buffer.is_empty() {
                    return Ok(None);
                }
                if buffer.last() == Some(&b'\r') {
                    buffer.pop();
                }
                return Ok(Some(FrameRead::Ready));
            }

            let newline = available.iter().position(|byte| *byte == b'\n');
            let data_len = newline.unwrap_or(available.len());
            if !oversized {
                if buffer.len().saturating_add(data_len) > MAX_FRAME_BYTES {
                    oversized = true;
                } else {
                    buffer.extend_from_slice(&available[..data_len]);
                }
            }
            (
                newline.map_or(available.len(), |index| index + 1),
                newline.is_some(),
            )
        };
        reader.consume(consumed);

        if has_newline {
            if oversized {
                return Ok(Some(FrameRead::TooLarge));
            }
            if buffer.last() == Some(&b'\r') {
                buffer.pop();
            }
            return Ok(Some(FrameRead::Ready));
        }
    }
}

async fn write_stdout(mut rx: mpsc::UnboundedReceiver<Value>) -> std::io::Result<()> {
    let mut stdout = tokio::io::stdout();
    while let Some(message) = rx.recv().await {
        let mut bytes = serde_json::to_vec(&message).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        stdout.write_all(&bytes).await?;
        stdout.flush().await?;
    }
    Ok(())
}

struct AppServer {
    store: SessionStore,
    cache: ModelsCache,
    http: reqwest::Client,
    outbound: Outbound,
    pending: PendingInteractions,
    request_seq: Arc<AtomicU64>,
    id_seq: AtomicU64,
    initialized: bool,
    draining: bool,
    threads: HashMap<String, Arc<ThreadRuntime>>,
    background_requests: Vec<tokio::task::JoinHandle<()>>,
}

impl AppServer {
    fn new(store: SessionStore, cache: ModelsCache, outbound: Outbound) -> Self {
        Self {
            store,
            cache,
            http: reqwest::Client::new(),
            outbound,
            pending: PendingInteractions::default(),
            request_seq: Arc::new(AtomicU64::new(0)),
            id_seq: AtomicU64::new(0),
            initialized: false,
            draining: false,
            threads: HashMap::new(),
            background_requests: Vec::new(),
        }
    }

    async fn handle_line(&mut self, line: &str) -> bool {
        if line.trim().is_empty() {
            return true;
        }
        if line.len() > MAX_FRAME_BYTES {
            self.send_error(
                Some(Value::Null),
                RpcFailure::new(protocol::INVALID_REQUEST, "JSON-RPC frame is too large"),
            );
            return true;
        }
        let message: IncomingMessage = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(error) => {
                self.send_error(
                    Some(Value::Null),
                    RpcFailure::new(protocol::INVALID_REQUEST, format!("invalid JSON: {error}")),
                );
                return true;
            }
        };
        if message.jsonrpc.as_deref() != Some("2.0") {
            self.send_error(
                message.id,
                RpcFailure::new(protocol::INVALID_REQUEST, "jsonrpc must be `2.0`"),
            );
            return true;
        }
        if message.method.is_none() {
            if let Some(id) = message.id.as_ref() {
                match ui::resolve_interaction_response(
                    &self.pending,
                    id,
                    message.result.as_ref(),
                    message.error.as_ref(),
                ) {
                    Ok(true) => {}
                    Ok(false) => eprintln!("aivo app-server: response for unknown id {id}"),
                    Err(error) => {
                        eprintln!("aivo app-server: invalid interaction response: {error}")
                    }
                }
            }
            return true;
        }
        if message
            .id
            .as_ref()
            .is_some_and(|id| !protocol::valid_rpc_id(id))
        {
            self.send_error(
                Some(Value::Null),
                RpcFailure::new(protocol::INVALID_REQUEST, "invalid JSON-RPC id"),
            );
            return true;
        }

        self.handle_request(
            message.id,
            message.method.unwrap_or_default(),
            message.params,
        )
        .await
    }

    async fn handle_request(
        &mut self,
        id: Option<Value>,
        method: String,
        params: Option<Value>,
    ) -> bool {
        match method.as_str() {
            "health/check" => {
                self.send_result(
                    id,
                    json!({
                        "state": if self.draining { "draining" } else if self.initialized { "ready" } else { "starting" },
                        "initialized": self.initialized,
                        "protocolVersion": protocol::PROTOCOL_VERSION,
                        "threads": self.threads.len(),
                        "pid": std::process::id(),
                    }),
                );
            }
            "initialize" => {
                let parsed = protocol::parse_params::<InitializeParams>(params);
                match parsed {
                    Ok(params) if params.protocol_version == protocol::PROTOCOL_VERSION => {
                        self.initialized = true;
                        let client = params
                            .client_info
                            .map(|info| json!({ "name": info.name, "version": info.version }));
                        self.send_result(
                            id,
                            json!({
                                "protocolVersion": protocol::PROTOCOL_VERSION,
                                "serverInfo": {
                                    "name": "aivo-app-server",
                                    "version": crate::version::VERSION,
                                },
                                "clientInfo": client,
                                "capabilities": {
                                    "threads": {
                                        "persistent": true,
                                        "list": true,
                                        "resume": true,
                                        "delete": true,
                                        "flush": true,
                                        "close": true,
                                        "maxActiveTurnsPerThread": 1
                                    },
                                    "turns": { "cancel": true, "textOnly": true },
                                    "models": {
                                        "providers": true,
                                        "list": true,
                                        "threadSelection": true
                                    },
                                    "approval": true,
                                    "userInput": true,
                                    "toolSources": {
                                        "productTools": {
                                            "managed": true,
                                            "configuration": "launcher",
                                            "transport": "stdio",
                                            "approvalRequired": true,
                                            "load": "thread",
                                            "bestEffort": true
                                        },
                                        "userMcp": {
                                            "tools": true,
                                            "configScopes": ["user"],
                                            "projectConfiguration": false,
                                            "transports": ["stdio", "streamableHttp"],
                                            "oauth": {
                                                "storedCredentials": true,
                                                "interactive": false
                                            },
                                            "load": "thread",
                                            "bestEffort": true
                                        }
                                    },
                                    "cloud": false,
                                },
                            }),
                        );
                    }
                    Ok(params) => self.send_error(
                        id,
                        RpcFailure::new(
                            UNSUPPORTED_VERSION,
                            format!("unsupported protocol version {}", params.protocol_version),
                        )
                        .with_data(json!({ "supported": [protocol::PROTOCOL_VERSION] })),
                    ),
                    Err(error) => self.send_error(id, error),
                }
            }
            "provider/list" => {
                if !self.require_initialized(id.clone()) {
                    return true;
                }
                match self.provider_list().await {
                    Ok(result) => self.send_result(id, result),
                    Err(error) => self.send_error(id, error),
                }
            }
            "model/list" => {
                if !self.require_initialized(id.clone()) {
                    return true;
                }
                match protocol::parse_params::<ModelListParams>(params) {
                    Ok(params) => {
                        if let Some(response_id) = id {
                            let store = self.store.clone();
                            let cache = self.cache.clone();
                            let http = self.http.clone();
                            let outbound = self.outbound.clone();
                            self.background_requests.retain(|task| !task.is_finished());
                            self.background_requests.push(tokio::spawn(async move {
                                let response = match Self::model_list(
                                    &store,
                                    &cache,
                                    &http,
                                    params,
                                )
                                .await
                                {
                                    Ok(result) => protocol::response(response_id, result),
                                    Err(error) => protocol::error_response(response_id, &error),
                                };
                                let _ = outbound.send(response);
                            }));
                        }
                    }
                    Err(error) => self.send_error(id, error),
                }
            }
            "thread/list" => {
                if !self.require_initialized(id.clone()) {
                    return true;
                }
                match protocol::parse_params::<ThreadListParams>(params) {
                    Ok(params) => match session::canonicalize_cwd(&params.cwd) {
                        Ok(cwd) => match self.store.all_chat_sessions().await {
                            Ok(mut entries) => {
                                let providers_by_key: HashMap<_, _> = self
                                    .store
                                    .get_keys_and_active_id_info()
                                    .await
                                    .map(|(keys, _)| {
                                        keys.into_iter()
                                            .map(|key| {
                                                let provider =
                                                    session::public_provider_for_base_url(
                                                        &key.base_url,
                                                    );
                                                (key.id, provider)
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                entries.retain(|entry| {
                                    std::fs::canonicalize(&entry.cwd)
                                        .is_ok_and(|entry_cwd| entry_cwd == cwd)
                                });
                                entries
                                    .sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
                                let cwd = cwd.to_string_lossy().to_string();
                                let mut data = Vec::with_capacity(entries.len());
                                for entry in entries {
                                    let provider = providers_by_key
                                        .get(&entry.key_id)
                                        .cloned()
                                        .unwrap_or_else(|| {
                                            session::public_provider_for_base_url(&entry.base_url)
                                        });
                                    let title = match self
                                        .store
                                        .get_code_session(&entry.session_id)
                                        .await
                                    {
                                        Ok(Some(state)) => session::title_from_messages(
                                            &state.messages,
                                            &state.model,
                                        ),
                                        _ => entry.title.clone(),
                                    };
                                    data.push(json!({
                                        "sessionId": entry.session_id,
                                        "cwd": cwd,
                                        "provider": provider,
                                        "model": entry.model,
                                        "title": title,
                                        "preview": entry.preview,
                                        "updatedAt": entry.updated_at,
                                        "createdAt": entry.created_at,
                                    }));
                                }
                                self.send_result(id, json!({ "data": data }));
                            }
                            Err(error) => self.send_error(
                                id,
                                RpcFailure::new(protocol::INTERNAL_ERROR, error.to_string()),
                            ),
                        },
                        Err(error) => self.send_error(id, error),
                    },
                    Err(error) => self.send_error(id, error),
                }
            }
            "thread/start" => {
                if !self.require_initialized(id.clone()) {
                    return true;
                }
                match protocol::parse_params::<ThreadStartParams>(params) {
                    Ok(params) => {
                        let model_provider = params.model_provider.or(params.key_id);
                        let thread_id = self.next_id("thread");
                        let session_id = new_durable_session_id();
                        let lease = match self.store.try_acquire_code_session_lease(&session_id) {
                            Ok(Some(lease)) => lease,
                            Ok(None) => {
                                self.send_error(
                                    id,
                                    RpcFailure::new(
                                        protocol::THREAD_BUSY,
                                        "new session id is unexpectedly leased",
                                    ),
                                );
                                return true;
                            }
                            Err(error) => {
                                self.send_error(
                                    id,
                                    RpcFailure::new(protocol::INTERNAL_ERROR, error.to_string()),
                                );
                                return true;
                            }
                        };
                        match ThreadRuntime::create(
                            thread_id.clone(),
                            session_id,
                            lease,
                            params.cwd,
                            model_provider,
                            params.model,
                            self.store.clone(),
                            &self.cache,
                        )
                        .await
                        {
                            Ok(thread) => match thread.persist_empty().await {
                                Ok(()) => {
                                    let result = json!({
                                        "threadId": thread.id.clone(),
                                        "sessionId": thread.session_id.clone(),
                                        "cwd": thread.cwd.clone(),
                                        "provider": thread.provider(),
                                        "model": thread.raw_model.clone(),
                                        "toolSources": thread.tool_sources(),
                                        "title": thread.title().await,
                                        "state": "idle",
                                    });
                                    self.threads.insert(thread_id, thread);
                                    self.send_result(id, result);
                                }
                                Err(error) => {
                                    let _ =
                                        self.store.delete_chat_session(&thread.session_id).await;
                                    self.send_error(id, error);
                                }
                            },
                            Err(error) => self.send_error(id, error),
                        }
                    }
                    Err(error) => self.send_error(id, error),
                }
            }
            "thread/resume" => {
                if !self.require_initialized(id.clone()) {
                    return true;
                }
                match protocol::parse_params::<ThreadResumeParams>(params) {
                    Ok(params) if !valid_session_id(&params.session_id) => self.send_error(
                        id,
                        RpcFailure::new(protocol::INVALID_PARAMS, "invalid session id"),
                    ),
                    Ok(params)
                        if self
                            .threads
                            .values()
                            .any(|thread| thread.session_id == params.session_id) =>
                    {
                        self.send_error(
                            id,
                            RpcFailure::new(protocol::THREAD_BUSY, "session is already loaded")
                                .with_data(json!({ "sessionId": params.session_id })),
                        );
                    }
                    Ok(params) => {
                        let lease = match self
                            .store
                            .try_acquire_code_session_lease(&params.session_id)
                        {
                            Ok(Some(lease)) => lease,
                            Ok(None) => {
                                self.send_error(
                                    id,
                                    RpcFailure::new(
                                        protocol::THREAD_BUSY,
                                        "session is already loaded by another process",
                                    )
                                    .with_data(json!({ "sessionId": params.session_id })),
                                );
                                return true;
                            }
                            Err(error) => {
                                self.send_error(
                                    id,
                                    RpcFailure::new(protocol::INTERNAL_ERROR, error.to_string()),
                                );
                                return true;
                            }
                        };
                        match self.store.get_code_session(&params.session_id).await {
                            Ok(Some(state)) if state.session_id != params.session_id => {
                                self.send_error(
                                    id,
                                    RpcFailure::new(
                                        protocol::INTERNAL_ERROR,
                                        "stored session id does not match its file name",
                                    ),
                                );
                            }
                            Ok(Some(state)) => {
                                let title =
                                    session::title_from_messages(&state.messages, &state.model);
                                let messages = public_messages(&state.messages);
                                let thread_id = self.next_id("thread");
                                match ThreadRuntime::resume(
                                    thread_id.clone(),
                                    state,
                                    title.clone(),
                                    lease,
                                    self.store.clone(),
                                    &self.cache,
                                )
                                .await
                                {
                                    Ok(thread) => {
                                        let result = json!({
                                            "threadId": thread.id.clone(),
                                            "sessionId": thread.session_id.clone(),
                                            "cwd": thread.cwd.clone(),
                                            "provider": thread.provider(),
                                            "model": thread.raw_model.clone(),
                                            "toolSources": thread.tool_sources(),
                                            "title": title,
                                            "messages": messages,
                                            "state": "idle",
                                        });
                                        self.threads.insert(thread_id, thread);
                                        self.send_result(id, result);
                                    }
                                    Err(error) if error.code == protocol::NOT_FOUND => self
                                        .send_error(
                                            id,
                                            RpcFailure::new(
                                                protocol::UNAVAILABLE,
                                                "session credentials are unavailable",
                                            ),
                                        ),
                                    Err(error) => self.send_error(id, error),
                                }
                            }
                            Ok(None) => self.send_error(
                                id,
                                RpcFailure::new(protocol::NOT_FOUND, "session not found"),
                            ),
                            Err(error) => self.send_error(
                                id,
                                RpcFailure::new(protocol::INTERNAL_ERROR, error.to_string()),
                            ),
                        }
                    }
                    Err(error) => self.send_error(id, error),
                }
            }
            "thread/delete" => {
                if !self.require_initialized(id.clone()) {
                    return true;
                }
                match protocol::parse_params::<ThreadDeleteParams>(params) {
                    Ok(params) if !valid_session_id(&params.session_id) => self.send_error(
                        id,
                        RpcFailure::new(protocol::INVALID_PARAMS, "invalid session id"),
                    ),
                    Ok(params)
                        if self
                            .threads
                            .values()
                            .any(|thread| thread.session_id == params.session_id) =>
                    {
                        self.send_error(
                            id,
                            RpcFailure::new(protocol::THREAD_BUSY, "session is currently loaded")
                                .with_data(json!({ "sessionId": params.session_id })),
                        );
                    }
                    Ok(params) => {
                        let _lease = match self
                            .store
                            .try_acquire_code_session_lease(&params.session_id)
                        {
                            Ok(Some(lease)) => lease,
                            Ok(None) => {
                                self.send_error(
                                    id,
                                    RpcFailure::new(
                                        protocol::THREAD_BUSY,
                                        "session is currently loaded by another process",
                                    )
                                    .with_data(json!({ "sessionId": params.session_id })),
                                );
                                return true;
                            }
                            Err(error) => {
                                self.send_error(
                                    id,
                                    RpcFailure::new(protocol::INTERNAL_ERROR, error.to_string()),
                                );
                                return true;
                            }
                        };
                        match self.store.delete_chat_session(&params.session_id).await {
                            Ok(deleted) => self.send_result(
                                id,
                                json!({
                                    "sessionId": params.session_id,
                                    "state": if deleted { "deleted" } else { "not_found" },
                                }),
                            ),
                            Err(error) => self.send_error(
                                id,
                                RpcFailure::new(protocol::INTERNAL_ERROR, error.to_string()),
                            ),
                        }
                    }
                    Err(error) => self.send_error(id, error),
                }
            }
            "thread/flush" => {
                if !self.require_initialized(id.clone()) {
                    return true;
                }
                match protocol::parse_params::<ThreadFlushParams>(params) {
                    Ok(params) => match self.threads.get(&params.thread_id).cloned() {
                        Some(thread) => match thread.flush().await {
                            Ok(()) => self.send_result(
                                id,
                                json!({
                                    "threadId": thread.id.clone(),
                                    "sessionId": thread.session_id.clone(),
                                    "persisted": true,
                                }),
                            ),
                            Err(error) => self.send_error(id, error),
                        },
                        None => self.send_error(
                            id,
                            RpcFailure::new(protocol::NOT_FOUND, "thread not found"),
                        ),
                    },
                    Err(error) => self.send_error(id, error),
                }
            }
            "thread/close" => {
                if !self.require_initialized(id.clone()) {
                    return true;
                }
                match protocol::parse_params::<ThreadCloseParams>(params) {
                    Ok(params) => match self.threads.remove(&params.thread_id) {
                        Some(thread) => {
                            thread.shutdown(&self.outbound, &self.pending).await;
                            self.send_result(
                                id,
                                json!({ "threadId": params.thread_id, "state": "closed" }),
                            );
                        }
                        None => self.send_error(
                            id,
                            RpcFailure::new(protocol::NOT_FOUND, "thread not found"),
                        ),
                    },
                    Err(error) => self.send_error(id, error),
                }
            }
            "turn/start" => {
                if !self.require_initialized(id.clone()) {
                    return true;
                }
                match protocol::parse_params::<TurnStartParams>(params) {
                    Ok(params) => match self.threads.get(&params.thread_id).cloned() {
                        Some(thread) => {
                            let turn_id = self.next_id("turn");
                            match thread
                                .prepare_turn(
                                    turn_id,
                                    params.text,
                                    self.outbound.clone(),
                                    self.pending.clone(),
                                    self.request_seq.clone(),
                                )
                                .await
                            {
                                Ok(prepared) => {
                                    self.send_result(
                                        id,
                                        json!({ "turnId": prepared.turn_id, "state": "running" }),
                                    );
                                    prepared.start();
                                }
                                Err(error) => self.send_error(id, error),
                            }
                        }
                        None => self.send_error(
                            id,
                            RpcFailure::new(protocol::NOT_FOUND, "thread not found"),
                        ),
                    },
                    Err(error) => self.send_error(id, error),
                }
            }
            "turn/cancel" => {
                if !self.require_initialized(id.clone()) {
                    return true;
                }
                match protocol::parse_params::<TurnCancelParams>(params) {
                    Ok(params) => match self.threads.get(&params.thread_id).cloned() {
                        Some(thread) => match thread.prepare_cancel(&params.turn_id).await {
                            Ok(Some(cancel)) => {
                                self.send_result(
                                    id,
                                    json!({ "turnId": params.turn_id, "state": "cancelled" }),
                                );
                                cancel.execute(&thread, &self.outbound, &self.pending).await;
                            }
                            Ok(None) => self.send_result(
                                id,
                                json!({ "turnId": params.turn_id, "state": "not_running" }),
                            ),
                            Err(error) => self.send_error(id, error),
                        },
                        None => self.send_error(
                            id,
                            RpcFailure::new(protocol::NOT_FOUND, "thread not found"),
                        ),
                    },
                    Err(error) => self.send_error(id, error),
                }
            }
            "shutdown" => {
                self.draining = true;
                self.send_result(id, json!({ "state": "draining" }));
                return false;
            }
            _ => self.send_error(
                id,
                RpcFailure::new(
                    protocol::METHOD_NOT_FOUND,
                    format!("method not found: {method}"),
                ),
            ),
        }
        true
    }

    fn require_initialized(&self, id: Option<Value>) -> bool {
        if self.initialized {
            true
        } else {
            self.send_error(
                id,
                RpcFailure::new(NOT_INITIALIZED, "initialize must be called first"),
            );
            false
        }
    }

    async fn provider_list(&self) -> Result<Value, RpcFailure> {
        let (keys, active_model_provider) = self
            .store
            .get_keys_and_active_id_info()
            .await
            .map_err(|error| RpcFailure::new(protocol::INTERNAL_ERROR, error.to_string()))?;
        let mut data = Vec::with_capacity(keys.len());
        for key in keys {
            let provider_id = key.id.clone();
            let display_name = key.display_name().to_string();
            let selected_model = self
                .store
                .get_code_model(&provider_id)
                .await
                .map_err(|error| RpcFailure::new(protocol::INTERNAL_ERROR, error.to_string()))?;
            let agent_compatible = !key.is_any_oauth() && !key.is_cursor_acp();
            let public_provider = session::public_provider_for_base_url(&key.base_url);
            data.push(json!({
                "id": provider_id,
                "displayName": display_name,
                "kind": public_provider.kind,
                "configurationLocation": public_provider.configuration_location,
                "inferenceLocation": public_provider.inference_location,
                "active": active_model_provider.as_deref() == Some(key.id.as_str()),
                "agentCompatible": agent_compatible,
                "selectedModel": selected_model,
            }));
        }
        Ok(json!({
            "activeModelProvider": active_model_provider,
            "data": data,
        }))
    }

    async fn model_list(
        store: &SessionStore,
        cache: &ModelsCache,
        http: &reqwest::Client,
        params: ModelListParams,
    ) -> Result<Value, RpcFailure> {
        let key = match params.model_provider.as_deref() {
            Some(provider) => store
                .resolve_key_by_id_or_name(provider)
                .await
                .map_err(|_| RpcFailure::new(protocol::NOT_FOUND, "model provider not found"))?,
            None => store
                .get_active_key()
                .await
                .map_err(|error| RpcFailure::new(protocol::INTERNAL_ERROR, error.to_string()))?
                .ok_or_else(|| RpcFailure::new(protocol::UNAVAILABLE, "no active model provider"))?,
        };
        if key.is_any_oauth() || key.is_cursor_acp() {
            return Err(RpcFailure::new(
                protocol::UNAVAILABLE,
                "the selected provider cannot drive the in-process AgentEngine",
            ));
        }

        let selected_model = store
            .get_code_model(&key.id)
            .await
            .map_err(|error| RpcFailure::new(protocol::INTERNAL_ERROR, error.to_string()))?;
        let (mut data, warning, catalog_available) =
            match crate::commands::models::fetch_models_cached(
                http,
                &key,
                cache,
                params.refresh,
            )
            .await
            {
                Ok(models) => (models, None, true),
                Err(_) => (
                    Vec::new(),
                    Some("the provider did not return a model catalog".to_string()),
                    false,
                ),
            };
        if !catalog_available
            && let Some(model) = selected_model.as_ref()
            && !data.contains(model)
        {
            data.insert(0, model.clone());
        }
        let selected_model_available = catalog_available
            .then(|| selected_model.as_ref().map(|model| data.contains(model)))
            .flatten();
        let provider_id = key.id.clone();
        let provider_name = key.display_name().to_string();
        Ok(json!({
            "modelProvider": provider_id,
            "providerName": provider_name,
            "selectedModel": selected_model,
            "selectedModelAvailable": selected_model_available,
            "catalogAvailable": catalog_available,
            "data": data,
            "warning": warning,
        }))
    }

    fn send_result(&self, id: Option<Value>, result: Value) {
        if let Some(id) = id {
            let _ = self.outbound.send(protocol::response(id, result));
        }
    }

    fn send_error(&self, id: Option<Value>, error: RpcFailure) {
        if let Some(id) = id {
            let _ = self.outbound.send(protocol::error_response(id, &error));
        }
    }

    fn next_id(&self, prefix: &str) -> String {
        let seq = self.id_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        format!("{prefix}_{nanos:x}_{seq:x}")
    }

    async fn shutdown(&mut self) {
        self.draining = true;
        for request in self.background_requests.drain(..) {
            request.abort();
            let _ = request.await;
        }
        for thread in self.threads.values() {
            thread.shutdown(&self.outbound, &self.pending).await;
        }
        ui::fail_all_pending(&self.pending);
        self.threads.clear();
    }
}

fn valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn new_durable_session_id() -> String {
    use rand::Rng;

    let bytes: [u8; 16] = rand::thread_rng().r#gen();
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u16::from_be_bytes([bytes[4], bytes[5]]),
        u16::from_be_bytes([bytes[6], bytes[7]]),
        u16::from_be_bytes([bytes[8], bytes[9]]),
        u64::from_be_bytes([
            0, 0, bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
        ]),
    )
}

fn public_messages(messages: &[StoredChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .filter(|message| matches!(message.role.as_str(), "user" | "assistant"))
        .map(|message| {
            let mut value = json!({
                "role": message.role,
                "content": message.content,
            });
            if let Some(reasoning) = message.reasoning_content.as_deref() {
                value["reasoningContent"] = Value::String(reasoning.to_string());
            }
            value
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn oversized_frame_is_drained_before_reading_the_next_frame() {
        let mut input = vec![b'x'; MAX_FRAME_BYTES + 1];
        input.extend_from_slice(b"\n{\"jsonrpc\":\"2.0\"}\n");
        let mut reader = BufReader::new(input.as_slice());
        let mut buffer = Vec::new();

        assert_eq!(
            read_frame(&mut reader, &mut buffer).await.unwrap(),
            Some(FrameRead::TooLarge)
        );
        assert!(buffer.len() <= MAX_FRAME_BYTES);
        assert_eq!(
            read_frame(&mut reader, &mut buffer).await.unwrap(),
            Some(FrameRead::Ready)
        );
        assert_eq!(buffer, br#"{"jsonrpc":"2.0"}"#);
        assert_eq!(read_frame(&mut reader, &mut buffer).await.unwrap(), None);
    }

    #[tokio::test]
    async fn health_works_before_initialize() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::with_path(dir.path().join("config.json"));
        let cache = ModelsCache::with_path(dir.path().join("models.json"));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut server = AppServer::new(store, cache, tx);
        assert!(
            server
                .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"health/check","params":{}}"#,)
                .await
        );
        let message = rx.recv().await.unwrap();
        assert_eq!(message["id"], 1);
        assert_eq!(message["result"]["state"], "starting");
    }

    #[tokio::test]
    async fn rejects_work_before_initialize() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::with_path(dir.path().join("config.json"));
        let cache = ModelsCache::with_path(dir.path().join("models.json"));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut server = AppServer::new(store, cache, tx);
        server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":"x","method":"thread/start","params":{"cwd":"."}}"#,
            )
            .await;
        let message = rx.recv().await.unwrap();
        assert_eq!(message["error"]["code"], NOT_INITIALIZED);
    }
}
