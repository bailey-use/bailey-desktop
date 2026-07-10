use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::Value;

#[test]
fn stdio_is_json_rpc_only_and_shutdown_flushes() {
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_aivo"))
        .args(["app-server", "--stdio"])
        .env("HOME", home.path())
        .env("AIVO_TEST_FAST_CRYPTO_OK", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1,"clientInfo":{{"name":"test","version":"1"}}}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"health/check","params":{{}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":3,"method":"shutdown","params":{{}}}}"#
    )
    .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let messages = stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("stdout must be JSON-RPC only"))
        .collect::<Vec<_>>();
    assert_eq!(messages.len(), 3, "stdout: {stdout}");
    assert_eq!(messages[0]["id"], 1);
    assert_eq!(messages[0]["result"]["protocolVersion"], 1);
    assert_eq!(messages[1]["id"], 2);
    assert_eq!(messages[1]["result"]["state"], "ready");
    assert_eq!(messages[2]["id"], 3);
    assert_eq!(messages[2]["result"]["state"], "draining");
}

#[test]
fn oversized_frame_is_rejected_without_losing_the_next_request() {
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_aivo"))
        .args(["app-server", "--stdio"])
        .env("HOME", home.path())
        .env("AIVO_TEST_FAST_CRYPTO_OK", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    stdin.write_all(&vec![b'x'; 1024 * 1024 + 1]).unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": 1}
        }),
    );

    let oversized = read(&mut stdout);
    assert_eq!(oversized["id"], Value::Null);
    assert_eq!(oversized["error"]["code"], -32600);
    assert_eq!(read(&mut stdout)["id"], 1);

    send(
        &mut stdin,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"shutdown","params":{}}),
    );
    assert_eq!(read(&mut stdout)["id"], 2);
    drop(stdin);
    drop(stdout);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn real_agent_engine_turn_streams_after_the_ack() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let script_dir = tempfile::tempdir().unwrap();
    let script = script_dir.path().join("model.json");
    std::fs::write(&script, r#"[{"text":"hello from the real engine"}]"#).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_aivo"))
        .args(["app-server", "--stdio"])
        .env("HOME", home.path())
        .env("AIVO_TEST_FAST_CRYPTO_OK", "1")
        .env("AIVO_AGENT_FAKE_SSE", &script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": 1}
        }),
    );
    assert_eq!(read(&mut stdout)["id"], 1);

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "thread/start",
            "params": {
                "cwd": workspace.path(),
                "model": "aivo/starter"
            }
        }),
    );
    let thread = read(&mut stdout);
    assert_eq!(thread["id"], 2);
    let thread_id = thread["result"]["threadId"].as_str().unwrap();

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "turn/start",
            "params": {"threadId": thread_id, "text": "say hello"}
        }),
    );
    let ack = read(&mut stdout);
    assert_eq!(ack["id"], 3, "turn/start must ack before events: {ack}");
    let turn_id = ack["result"]["turnId"].as_str().unwrap().to_string();

    let mut event_types = Vec::new();
    let mut text = String::new();
    loop {
        let message = read(&mut stdout);
        assert_eq!(message["method"], "event");
        assert_eq!(message["params"]["threadId"], thread_id);
        assert_eq!(message["params"]["turnId"], turn_id);
        let event_type = message["params"]["type"].as_str().unwrap().to_string();
        if event_type == "assistant.text.delta" {
            text.push_str(message["params"]["payload"]["text"].as_str().unwrap());
        }
        event_types.push(event_type.clone());
        if event_type == "turn.completed" {
            break;
        }
    }
    assert_eq!(
        event_types.first().map(String::as_str),
        Some("turn.started")
    );
    assert!(event_types.contains(&"assistant.text.delta".to_string()));
    assert_eq!(
        event_types.last().map(String::as_str),
        Some("turn.completed")
    );
    assert_eq!(text, "hello from the real engine");

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "thread/close",
            "params": {"threadId": thread_id}
        }),
    );
    let closed = read(&mut stdout);
    assert_eq!(closed["id"], 4);
    assert_eq!(closed["result"]["state"], "closed");

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "health/check",
            "params": {}
        }),
    );
    assert_eq!(read(&mut stdout)["result"]["threads"], 0);

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "shutdown",
            "params": {}
        }),
    );
    assert_eq!(read(&mut stdout)["id"], 6);
    drop(stdin);
    drop(stdout);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn tool_approval_round_trips_and_unblocks_the_engine() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("approval.txt"), "old").unwrap();
    let script_dir = tempfile::tempdir().unwrap();
    let script = script_dir.path().join("model.json");
    std::fs::write(
        &script,
        r#"[
          {"tools":[{"name":"write_file","args":{"path":"approval.txt","content":"approved"}}]},
          {"text":"the approved edit is complete"}
        ]"#,
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_aivo"))
        .args(["app-server", "--stdio"])
        .env("HOME", home.path())
        .env("AIVO_TEST_FAST_CRYPTO_OK", "1")
        .env("AIVO_AGENT_FAKE_SSE", &script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}
        }),
    );
    read(&mut stdout);
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"thread/start",
            "params":{"cwd":workspace.path(),"model":"aivo/starter"}
        }),
    );
    let thread = read(&mut stdout);
    let thread_id = thread["result"]["threadId"].as_str().unwrap();
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":3,"method":"turn/start",
            "params":{"threadId":thread_id,"text":"replace approval.txt"}
        }),
    );
    assert_eq!(read(&mut stdout)["id"], 3);

    let mut approved = false;
    loop {
        let message = read(&mut stdout);
        if message["method"] == "approval/request" {
            assert_eq!(message["params"]["subject"]["tool"], "write_file");
            let request_id = message["id"].clone();
            send(
                &mut stdin,
                serde_json::json!({
                    "jsonrpc":"2.0","id":request_id,"result":{"decision":"allow"}
                }),
            );
            approved = true;
            continue;
        }
        if message["method"] == "event" && message["params"]["type"] == "turn.completed" {
            break;
        }
    }
    assert!(approved, "the existing unread file must require approval");
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("approval.txt")).unwrap(),
        "approved"
    );

    send(
        &mut stdin,
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"shutdown","params":{}}),
    );
    assert_eq!(read(&mut stdout)["id"], 4);
    drop(stdin);
    drop(stdout);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cancelling_a_pending_approval_emits_cancelled_last_and_fails_closed() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("approval.txt"), "old").unwrap();
    let script_dir = tempfile::tempdir().unwrap();
    let script = script_dir.path().join("model.json");
    std::fs::write(
        &script,
        r#"[{"tools":[{"name":"write_file","args":{"path":"approval.txt","content":"must not run"}}]}]"#,
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_aivo"))
        .args(["app-server", "--stdio"])
        .env("HOME", home.path())
        .env("AIVO_TEST_FAST_CRYPTO_OK", "1")
        .env("AIVO_AGENT_FAKE_SSE", &script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}
        }),
    );
    read(&mut stdout);
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"thread/start",
            "params":{"cwd":workspace.path(),"model":"aivo/starter"}
        }),
    );
    let thread = read(&mut stdout);
    let thread_id = thread["result"]["threadId"].as_str().unwrap();
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":3,"method":"turn/start",
            "params":{"threadId":thread_id,"text":"replace approval.txt"}
        }),
    );
    let turn = read(&mut stdout);
    let turn_id = turn["result"]["turnId"].as_str().unwrap();

    let approval_id = loop {
        let message = read(&mut stdout);
        if message["method"] == "approval/request" {
            break message["id"].clone();
        }
    };
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":4,"method":"turn/cancel",
            "params":{"threadId":thread_id,"turnId":"not-the-active-turn"}
        }),
    );
    let wrong_cancel = read(&mut stdout);
    assert_eq!(wrong_cancel["id"], 4);
    assert_eq!(wrong_cancel["error"]["code"], -32004);

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":5,"method":"turn/cancel",
            "params":{"threadId":thread_id,"turnId":turn_id}
        }),
    );
    let cancel_ack = read(&mut stdout);
    assert_eq!(cancel_ack["id"], 5);
    assert_eq!(cancel_ack["result"]["state"], "cancelled");
    let terminal = read(&mut stdout);
    assert_eq!(terminal["method"], "event");
    assert_eq!(terminal["params"]["type"], "turn.cancelled");
    assert_eq!(terminal["params"]["turnId"], turn_id);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("approval.txt")).unwrap(),
        "old"
    );

    send(
        &mut stdin,
        serde_json::json!({"jsonrpc":"2.0","id":approval_id,"result":{"decision":"allow"}}),
    );
    send(
        &mut stdin,
        serde_json::json!({"jsonrpc":"2.0","id":6,"method":"health/check","params":{}}),
    );
    assert_eq!(read(&mut stdout)["id"], 6, "no event may follow cancelled");

    send(
        &mut stdin,
        serde_json::json!({"jsonrpc":"2.0","id":7,"method":"shutdown","params":{}}),
    );
    assert_eq!(read(&mut stdout)["id"], 7);
    drop(stdin);
    drop(stdout);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn send(stdin: &mut impl Write, message: Value) {
    writeln!(stdin, "{message}").unwrap();
    stdin.flush().unwrap();
}

fn read(stdout: &mut impl BufRead) -> Value {
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert!(!line.is_empty(), "app-server closed stdout unexpectedly");
    serde_json::from_str(&line).unwrap()
}
