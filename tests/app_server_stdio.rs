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
    let thread_id = thread["result"]["threadId"].as_str().unwrap().to_string();
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
    let thread_id = thread["result"]["threadId"].as_str().unwrap().to_string();
    let session_id = thread["result"]["sessionId"].as_str().unwrap().to_string();
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
            "jsonrpc":"2.0","id":45,"method":"thread/flush",
            "params":{"threadId":thread_id}
        }),
    );
    let busy_flush = read(&mut stdout);
    assert_eq!(busy_flush["id"], 45);
    assert_eq!(busy_flush["error"]["code"], -32003);
    assert_eq!(busy_flush["error"]["data"]["turnId"], turn_id);

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
    assert_eq!(terminal["params"]["payload"]["persisted"], true);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("approval.txt")).unwrap(),
        "old"
    );
    let stored: Value = serde_json::from_str(
        &std::fs::read_to_string(
            home.path()
                .join(".config/aivo/sessions")
                .join(format!("{session_id}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(stored["messages"].as_array().unwrap().len(), 1);
    assert_eq!(stored["messages"][0]["role"], "user");
    assert_eq!(stored["messages"][0]["content"], "replace approval.txt");
    let engine = stored["engineMessages"].as_array().unwrap();
    assert!(engine.iter().any(|message| message["role"] == "user"));
    assert!(
        engine
            .iter()
            .any(|message| { message["role"] == "assistant" && message["tool_calls"].is_array() })
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

#[test]
fn durable_threads_list_resume_and_preserve_tokens_across_restart() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let canonical_cwd = std::fs::canonicalize(workspace.path()).unwrap();
    let script_dir = tempfile::tempdir().unwrap();
    let script = script_dir.path().join("model.json");
    std::fs::write(&script, r#"[{"text":"durable answer"}]"#).unwrap();

    let (mut child, mut stdin, mut stdout) = spawn_server(home.path(), &script);
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}
        }),
    );
    let initialized = read(&mut stdout);
    assert_eq!(
        initialized["result"]["capabilities"]["threads"]["persistent"],
        true
    );

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"thread/start",
            "params":{"cwd":workspace.path(),"model":"aivo/starter"}
        }),
    );
    let started = read(&mut stdout);
    let first_thread_id = started["result"]["threadId"].as_str().unwrap().to_string();
    let session_id = started["result"]["sessionId"].as_str().unwrap().to_string();
    assert_eq!(started["result"]["title"], "新任务");
    assert_eq!(
        started["result"]["cwd"],
        canonical_cwd.to_string_lossy().as_ref()
    );
    assert!(started["result"].get("keyName").is_none());
    assert!(started["result"].get("baseUrl").is_none());

    // `thread/start` is durable before the first turn, and cwd aliases canonicalize.
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":3,"method":"thread/list",
            "params":{"cwd":workspace.path().join(".")}
        }),
    );
    let empty_list = read(&mut stdout);
    assert_eq!(empty_list["result"]["data"].as_array().unwrap().len(), 1);
    assert_eq!(empty_list["result"]["data"][0]["sessionId"], session_id);
    assert_eq!(empty_list["result"]["data"][0]["title"], "新任务");
    assert!(empty_list["result"]["data"][0].get("keyId").is_none());
    assert!(empty_list["result"]["data"][0].get("baseUrl").is_none());

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":4,"method":"turn/start",
            "params":{"threadId":first_thread_id,"text":"Remember this task"}
        }),
    );
    assert_eq!(read(&mut stdout)["id"], 4);
    read_until_terminal(&mut stdout, "turn.completed");

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":5,"method":"thread/list",
            "params":{"cwd":workspace.path()}
        }),
    );
    let populated_list = read(&mut stdout);
    assert_eq!(
        populated_list["result"]["data"][0]["title"],
        "Remember this task"
    );
    assert!(
        populated_list["result"]["data"][0]["preview"]
            .as_str()
            .unwrap()
            .contains("durable answer")
    );

    // Closing unloads only the runtime; the durable row remains listable.
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":6,"method":"thread/close",
            "params":{"threadId":first_thread_id}
        }),
    );
    assert_eq!(read(&mut stdout)["result"]["state"], "closed");
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":7,"method":"thread/list",
            "params":{"cwd":workspace.path()}
        }),
    );
    assert_eq!(
        read(&mut stdout)["result"]["data"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    shutdown_server(&mut child, stdin, stdout, 8);

    // Seed non-zero historical totals to prove a resumed turn never resets them.
    let index_path = home.path().join(".config/aivo/sessions/index.json");
    let mut index: Value =
        serde_json::from_str(&std::fs::read_to_string(&index_path).unwrap()).unwrap();
    let entry = index["entries"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|entry| entry["session_id"] == session_id)
        .unwrap();
    entry["prompt_tokens"] = serde_json::json!(41);
    entry["completion_tokens"] = serde_json::json!(7);
    entry["cache_read_tokens"] = serde_json::json!(3);
    entry["cache_write_tokens"] = serde_json::json!(2);
    std::fs::write(&index_path, serde_json::to_vec_pretty(&index).unwrap()).unwrap();

    let (mut child, mut stdin, mut stdout) = spawn_server(home.path(), &script);
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
            "jsonrpc":"2.0","id":2,"method":"thread/list",
            "params":{"cwd":workspace.path()}
        }),
    );
    assert_eq!(
        read(&mut stdout)["result"]["data"][0]["sessionId"],
        session_id
    );

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":3,"method":"thread/resume",
            "params":{"sessionId":"session_does_not_exist"}
        }),
    );
    assert_eq!(read(&mut stdout)["error"]["code"], -32004);

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":4,"method":"thread/resume",
            "params":{"sessionId":session_id}
        }),
    );
    let resumed = read(&mut stdout);
    let resumed_thread_id = resumed["result"]["threadId"].as_str().unwrap().to_string();
    assert_ne!(resumed_thread_id, first_thread_id);
    assert_eq!(resumed["result"]["sessionId"], session_id);
    assert_eq!(resumed["result"]["title"], "Remember this task");
    assert_eq!(resumed["result"]["messages"][0]["role"], "user");
    assert_eq!(
        resumed["result"]["messages"][0]["content"],
        "Remember this task"
    );
    assert_eq!(resumed["result"]["messages"][1]["role"], "assistant");
    assert_eq!(
        resumed["result"]["messages"][1]["content"],
        "durable answer"
    );
    assert!(resumed["result"].get("keyName").is_none());
    assert!(resumed["result"].get("baseUrl").is_none());

    // The same durable session cannot have two writable runtimes.
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":5,"method":"thread/resume",
            "params":{"sessionId":session_id}
        }),
    );
    assert_eq!(read(&mut stdout)["error"]["code"], -32003);

    // A second app-server process observes the same kernel lease for resume/delete.
    let (mut competitor, mut competitor_stdin, mut competitor_stdout) =
        spawn_server(home.path(), &script);
    send(
        &mut competitor_stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":101,"method":"initialize","params":{"protocolVersion":1}
        }),
    );
    read(&mut competitor_stdout);
    send(
        &mut competitor_stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":102,"method":"thread/resume",
            "params":{"sessionId":session_id}
        }),
    );
    assert_eq!(read(&mut competitor_stdout)["error"]["code"], -32003);
    send(
        &mut competitor_stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":103,"method":"thread/delete",
            "params":{"sessionId":session_id}
        }),
    );
    assert_eq!(read(&mut competitor_stdout)["error"]["code"], -32003);

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":6,"method":"turn/start",
            "params":{"threadId":resumed_thread_id,"text":"Follow up after restart"}
        }),
    );
    assert_eq!(read(&mut stdout)["id"], 6);
    read_until_terminal(&mut stdout, "turn.completed");
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":7,"method":"thread/close",
            "params":{"threadId":resumed_thread_id}
        }),
    );
    assert_eq!(read(&mut stdout)["result"]["state"], "closed");

    // The other process can claim the released lease; SIGKILL then releases it
    // even though the stale lock file remains on disk.
    send(
        &mut competitor_stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":104,"method":"thread/resume",
            "params":{"sessionId":session_id}
        }),
    );
    assert!(read(&mut competitor_stdout)["result"]["threadId"].is_string());
    drop(competitor_stdin);
    drop(competitor_stdout);
    competitor.kill().unwrap();
    assert!(!competitor.wait().unwrap().success());

    // Once unloaded, the same session can be resumed again with both turns visible.
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":8,"method":"thread/resume",
            "params":{"sessionId":session_id}
        }),
    );
    let resumed_again = read(&mut stdout);
    let resumed_again_thread_id = resumed_again["result"]["threadId"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        resumed_again["result"]["messages"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        resumed_again["result"]["messages"][2]["content"],
        "Follow up after restart"
    );
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":90,"method":"thread/flush",
            "params":{"threadId":resumed_again_thread_id}
        }),
    );
    let flushed = read(&mut stdout);
    assert_eq!(flushed["result"]["persisted"], true);
    assert_eq!(flushed["result"]["sessionId"], session_id);

    // A fresh durable id after restart must never overwrite the resumed session.
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":9,"method":"thread/start",
            "params":{"cwd":workspace.path(),"model":"aivo/starter"}
        }),
    );
    let second_session = read(&mut stdout);
    let second_thread_id = second_session["result"]["threadId"]
        .as_str()
        .unwrap()
        .to_string();
    let second_session_id = second_session["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(second_session_id, session_id);

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":10,"method":"thread/delete",
            "params":{"sessionId":second_session_id}
        }),
    );
    assert_eq!(read(&mut stdout)["error"]["code"], -32003);
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":11,"method":"thread/close",
            "params":{"threadId":second_thread_id}
        }),
    );
    assert_eq!(read(&mut stdout)["result"]["state"], "closed");
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":12,"method":"thread/delete",
            "params":{"sessionId":second_session_id}
        }),
    );
    assert_eq!(read(&mut stdout)["result"]["state"], "deleted");
    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc":"2.0","id":13,"method":"thread/delete",
            "params":{"sessionId":second_session_id}
        }),
    );
    assert_eq!(read(&mut stdout)["result"]["state"], "not_found");
    shutdown_server(&mut child, stdin, stdout, 14);

    let index: Value = serde_json::from_str(&std::fs::read_to_string(index_path).unwrap()).unwrap();
    let entry = index["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["session_id"] == session_id)
        .unwrap();
    assert_eq!(entry["prompt_tokens"], 41);
    assert_eq!(entry["completion_tokens"], 7);
    assert_eq!(entry["cache_read_tokens"], 3);
    assert_eq!(entry["cache_write_tokens"], 2);
}

fn spawn_server(
    home: &std::path::Path,
    script: &std::path::Path,
) -> (
    std::process::Child,
    std::process::ChildStdin,
    BufReader<std::process::ChildStdout>,
) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_aivo"))
        .args(["app-server", "--stdio"])
        .env("HOME", home)
        .env("AIVO_TEST_FAST_CRYPTO_OK", "1")
        .env("AIVO_AGENT_FAKE_SSE", script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    (child, stdin, stdout)
}

fn read_until_terminal(stdout: &mut impl BufRead, expected: &str) {
    loop {
        let message = read(stdout);
        if message["method"] == "event" && message["params"]["type"] == expected {
            return;
        }
    }
}

fn shutdown_server(
    child: &mut std::process::Child,
    mut stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    id: i64,
) {
    send(
        &mut stdin,
        serde_json::json!({"jsonrpc":"2.0","id":id,"method":"shutdown","params":{}}),
    );
    let mut stdout = stdout;
    assert_eq!(read(&mut stdout)["id"], id);
    drop(stdin);
    drop(stdout);
    let status = child.wait().unwrap();
    assert!(status.success());
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
