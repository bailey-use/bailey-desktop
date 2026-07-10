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
use crate::services::{ModelsCache, SessionStore};

use protocol::{
    IncomingMessage, InitializeParams, NOT_INITIALIZED, RpcFailure, ThreadCloseParams,
    ThreadStartParams, TurnCancelParams, TurnStartParams, UNSUPPORTED_VERSION,
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
    outbound: Outbound,
    pending: PendingInteractions,
    request_seq: Arc<AtomicU64>,
    id_seq: AtomicU64,
    initialized: bool,
    draining: bool,
    threads: HashMap<String, Arc<ThreadRuntime>>,
}

impl AppServer {
    fn new(store: SessionStore, cache: ModelsCache, outbound: Outbound) -> Self {
        Self {
            store,
            cache,
            outbound,
            pending: PendingInteractions::default(),
            request_seq: Arc::new(AtomicU64::new(0)),
            id_seq: AtomicU64::new(0),
            initialized: false,
            draining: false,
            threads: HashMap::new(),
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
                                    "threads": { "persistent": false, "close": true, "maxActiveTurnsPerThread": 1 },
                                    "turns": { "cancel": true, "textOnly": true },
                                    "approval": true,
                                    "userInput": true,
                                    "mcp": false,
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
            "thread/start" => {
                if !self.require_initialized(id.clone()) {
                    return true;
                }
                match protocol::parse_params::<ThreadStartParams>(params) {
                    Ok(params) => {
                        let thread_id = self.next_id("thread");
                        match ThreadRuntime::create(
                            thread_id.clone(),
                            params.cwd,
                            params.key_id,
                            params.model,
                            self.store.clone(),
                            &self.cache,
                        )
                        .await
                        {
                            Ok(thread) => {
                                let result = json!({
                                    "threadId": thread.id.clone(),
                                    "cwd": thread.cwd.clone(),
                                    "model": thread.raw_model.clone(),
                                    "keyName": thread.key_name.clone(),
                                    "state": "idle",
                                });
                                self.threads.insert(thread_id, thread);
                                self.send_result(id, result);
                            }
                            Err(error) => self.send_error(id, error),
                        }
                    }
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
                                cancel.execute(&self.outbound, &self.pending).await;
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
        for thread in self.threads.values() {
            thread.shutdown(&self.outbound, &self.pending).await;
        }
        ui::fail_all_pending(&self.pending);
        self.threads.clear();
    }
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
