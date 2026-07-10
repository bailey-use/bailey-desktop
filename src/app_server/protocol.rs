use chrono::SecondsFormat;
use serde::Deserialize;
use serde_json::{Value, json};

pub const PROTOCOL_VERSION: u32 = 1;
pub const EVENT_SCHEMA_VERSION: u32 = 1;

pub const NOT_INITIALIZED: i64 = -32_001;
pub const UNSUPPORTED_VERSION: i64 = -32_002;
pub const THREAD_BUSY: i64 = -32_003;
pub const NOT_FOUND: i64 = -32_004;
pub const UNAVAILABLE: i64 = -32_005;

pub const INVALID_REQUEST: i64 = -32_600;
pub const METHOD_NOT_FOUND: i64 = -32_601;
pub const INVALID_PARAMS: i64 = -32_602;
pub const INTERNAL_ERROR: i64 = -32_603;

#[derive(Debug, Deserialize)]
pub struct IncomingMessage {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: u32,
    #[serde(default)]
    pub client_info: Option<ClientInfo>,
}

#[derive(Debug, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartParams {
    pub cwd: String,
    #[serde(default)]
    pub key_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCloseParams {
    pub thread_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    pub thread_id: String,
    pub text: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnCancelParams {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Debug)]
pub struct RpcFailure {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcFailure {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

pub fn response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

pub fn error_response(id: Value, failure: &RpcFailure) -> Value {
    let mut error = json!({
        "code": failure.code,
        "message": failure.message,
    });
    if let Some(data) = &failure.data {
        error["data"] = data.clone();
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": error,
    })
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
}

pub fn server_request(id: String, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

pub fn event(thread_id: &str, turn_id: &str, seq: u64, event_type: &str, payload: Value) -> Value {
    notification(
        "event",
        json!({
            "schemaVersion": EVENT_SCHEMA_VERSION,
            "seq": seq,
            "threadId": thread_id,
            "turnId": turn_id,
            "type": event_type,
            "createdAt": chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            "payload": payload,
        }),
    )
}

pub fn parse_params<T: for<'de> Deserialize<'de>>(params: Option<Value>) -> Result<T, RpcFailure> {
    serde_json::from_value(params.unwrap_or_else(|| json!({})))
        .map_err(|e| RpcFailure::new(INVALID_PARAMS, format!("invalid params: {e}")))
}

pub fn valid_rpc_id(id: &Value) -> bool {
    id.is_string() || id.is_number() || id.is_null()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_has_versioned_envelope() {
        let value = event(
            "thr_1",
            "turn_1",
            7,
            "tool.started",
            json!({"name": "read_file"}),
        );
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["method"], "event");
        assert_eq!(value["params"]["schemaVersion"], 1);
        assert_eq!(value["params"]["seq"], 7);
        assert_eq!(value["params"]["threadId"], "thr_1");
        assert_eq!(value["params"]["turnId"], "turn_1");
        assert_eq!(value["params"]["type"], "tool.started");
        assert!(value["params"]["createdAt"].as_str().is_some());
    }

    #[test]
    fn error_response_preserves_request_id_and_data() {
        let value = error_response(
            json!(42),
            &RpcFailure::new(THREAD_BUSY, "busy").with_data(json!({"threadId": "thr_1"})),
        );
        assert_eq!(value["id"], 42);
        assert_eq!(value["error"]["code"], THREAD_BUSY);
        assert_eq!(value["error"]["data"]["threadId"], "thr_1");
    }
}
