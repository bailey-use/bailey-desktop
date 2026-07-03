//! Deterministic scripted "model" for eval/CI. When `AIVO_AGENT_FAKE_SSE` points
//! at a script file, `aivo chat -e` talks to this loopback server instead of a real
//! provider, so the *real* agent loop and *real* tool execution run against a fixed
//! sequence of model turns — no tokens, no flakiness, gate-able in CI.
//!
//! Script format: a JSON array of turns, each either a tool-call batch or a final
//! answer. Turns are replayed one per model call, in order; once exhausted a
//! terminal answer is repeated so an over-long loop converges instead of erroring.
//!
//! ```json
//! [
//!   {"tools": [{"name": "edit_file", "args": {"path": "calc.sh", "old_string": "-", "new_string": "+"}}]},
//!   {"tools": [{"name": "run_bash", "args": {"command": "sh run_tests.sh"}}]},
//!   {"text": "Fixed the subtraction bug and verified the tests print ok."}
//! ]
//! ```

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use serde_json::{Value, json};

/// Convert one scripted turn to an OpenAI chat-completions SSE body the engine
/// consumes: `{"text": ...}` → a content delta, `{"tools": [...]}` → a tool-call
/// batch (each entry at its own `index`).
fn sse_body(turn: &Value) -> Result<String, String> {
    if let Some(text) = turn.get("text").and_then(Value::as_str) {
        let delta = json!({"choices": [{"delta": {"content": text}}]});
        return Ok(format!("data: {delta}\n\ndata: [DONE]\n\n"));
    }
    if let Some(tools) = turn.get("tools").and_then(Value::as_array) {
        if tools.is_empty() {
            return Err("fake_model: `tools` turn has no calls".to_string());
        }
        let entries: Vec<Value> = tools
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let name = t.get("name").and_then(Value::as_str).unwrap_or_default();
                let args = t.get("args").cloned().unwrap_or_else(|| json!({}));
                json!({
                    "index": i,
                    "id": format!("c{i}"),
                    "function": {"name": name, "arguments": args.to_string()},
                })
            })
            .collect();
        let delta = json!({"choices": [{"delta": {"tool_calls": entries}}]});
        return Ok(format!("data: {delta}\n\ndata: [DONE]\n\n"));
    }
    Err(format!(
        "fake_model: each turn needs `text` or `tools`, got: {turn}"
    ))
}

/// Served after the script is exhausted so an extra model call converges cleanly.
fn terminal_body() -> String {
    let delta = json!({"choices": [{"delta": {"content": "(scripted run complete)"}}]});
    format!("data: {delta}\n\ndata: [DONE]\n\n")
}

/// Parse a fake-model script (JSON array of turns) into ready-to-serve SSE bodies.
pub fn parse_script(json_str: &str) -> Result<Vec<String>, String> {
    let parsed: Value =
        serde_json::from_str(json_str).map_err(|e| format!("fake_model: invalid JSON: {e}"))?;
    let turns = parsed
        .as_array()
        .ok_or("fake_model: script must be a JSON array of turns")?;
    if turns.is_empty() {
        return Err("fake_model: script has no turns".to_string());
    }
    turns.iter().map(sse_body).collect()
}

/// Load and parse the script at `path`.
pub fn load_script(path: &str) -> Result<Vec<String>, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("fake_model: cannot read script {path}: {e}"))?;
    parse_script(&raw)
}

/// Best-effort read of the client request so it isn't blocked writing when we reply.
/// One read is enough: we never use the content, and for a loopback request the
/// headers (and any small body) arrive together; anything unread is discarded on
/// close. Mirrors the proven test SSE stub (`spawn_sse_sequence`).
fn drain_request(sock: &mut TcpStream) {
    let mut buf = [0u8; 16384];
    let _ = sock.read(&mut buf);
}

/// Start a background scripted-model server; returns its loopback port. Replays
/// `bodies` in order (one per request), repeating a terminal answer afterward.
/// The thread is detached and dies with the process — fine for a one-shot `-e` run.
pub fn start(bodies: Vec<String>) -> std::io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let trace = std::env::var("AIVO_FAKE_TRACE").is_ok();
    std::thread::spawn(move || {
        let terminal = terminal_body();
        let mut i = 0usize;
        loop {
            let Ok((mut sock, _)) = listener.accept() else {
                break;
            };
            if trace {
                eprintln!("[fake_model] accepted request {i}");
            }
            drain_request(&mut sock);
            let body = bodies.get(i).unwrap_or(&terminal);
            if trace {
                eprintln!(
                    "[fake_model] replying to request {i} ({} bytes)",
                    body.len()
                );
            }
            i += 1;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
        }
    });
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_turn_becomes_a_content_delta() {
        let body = sse_body(&json!({"text": "all done"})).unwrap();
        assert!(body.contains(r#""content":"all done""#));
        assert!(body.trim_end().ends_with("data: [DONE]"));
    }

    #[test]
    fn tools_turn_becomes_indexed_tool_calls() {
        let body = sse_body(&json!({"tools": [
            {"name": "run_bash", "args": {"command": "ls"}},
            {"name": "read_file", "args": {"path": "a.rs"}},
        ]}))
        .unwrap();
        assert!(body.contains(r#""name":"run_bash""#));
        assert!(body.contains(r#""index":0"#));
        assert!(body.contains(r#""name":"read_file""#));
        assert!(body.contains(r#""index":1"#));
        // args are serialized as a JSON string, as the OpenAI wire expects.
        assert!(body.contains(r#""arguments":"{\"command\":\"ls\"}""#));
    }

    /// Drive the real server over a loopback socket and read back the raw response.
    fn raw_post(port: u16) -> String {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(
            b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}",
        )
        .unwrap();
        s.flush().unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn server_replays_bodies_in_order_then_a_terminal_turn() {
        let bodies = parse_script(
            r#"[
                {"tools": [{"name": "run_bash", "args": {"command": "ls"}}]},
                {"text": "all done"}
            ]"#,
        )
        .unwrap();
        let port = start(bodies).unwrap();

        // Request 1 → the scripted tool call, framed as event-stream with a DONE.
        let r1 = raw_post(port);
        assert!(r1.starts_with("HTTP/1.1 200 OK"));
        assert!(r1.contains("text/event-stream"));
        assert!(r1.contains(r#""name":"run_bash""#));
        assert!(r1.contains("data: [DONE]"));

        // Request 2 → the scripted final answer.
        let r2 = raw_post(port);
        assert!(r2.contains(r#""content":"all done""#));

        // Request 3 → past the end, a terminal answer so an over-long loop converges.
        let r3 = raw_post(port);
        assert!(r3.contains("scripted run complete"));
    }

    #[test]
    fn parse_script_rejects_bad_shapes() {
        assert!(parse_script("not json").is_err());
        assert!(parse_script("{}").is_err()); // not an array
        assert!(parse_script("[]").is_err()); // empty
        assert!(parse_script(r#"[{"nope": 1}]"#).is_err()); // neither text nor tools
        assert!(parse_script(r#"[{"tools": []}]"#).is_err()); // empty batch
    }

    #[test]
    fn parse_script_accepts_a_valid_sequence() {
        let bodies = parse_script(
            r#"[
                {"tools": [{"name": "edit_file", "args": {"path": "x"}}]},
                {"text": "done"}
            ]"#,
        )
        .unwrap();
        assert_eq!(bodies.len(), 2);
        assert!(bodies[0].contains(r#""name":"edit_file""#));
        assert!(bodies[1].contains(r#""content":"done""#));
    }
}
