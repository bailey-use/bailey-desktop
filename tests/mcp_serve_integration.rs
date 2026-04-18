//! End-to-end integration test for `aivo mcp-serve`.
//!
//! Spawns the built `aivo` binary as a subprocess, seeds fixture JSONL
//! session files under a tempdir `HOME`, drives the server's stdin/stdout
//! with newline-delimited JSON-RPC, and asserts the full protocol flow.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

/// Returns the path to the aivo binary to test against. Prefers the current
/// cargo test target's debug binary.
fn aivo_exe() -> PathBuf {
    // `CARGO_BIN_EXE_<name>` is set by cargo for integration tests.
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_aivo") {
        return PathBuf::from(p);
    }
    // Fallback: <target>/debug/aivo
    let mut p = std::env::current_exe().unwrap();
    // target/debug/deps/xxx-hash
    p.pop(); // -> deps
    p.pop(); // -> debug
    p.push("aivo");
    p
}

/// Writes a fixture Claude session JSONL for the given encoded-cwd dir.
fn seed_claude(home: &Path, cwd: &str, session_id: &str, lines: &[&str]) {
    let encoded = cwd.replace('/', "-");
    let dir = home.join(".claude").join("projects").join(encoded);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{session_id}.jsonl"));
    std::fs::write(path, lines.join("\n")).unwrap();
}

/// Writes a fixture Codex rollout JSONL.
fn seed_codex(home: &Path, cwd: &str, session_id: &str, extra_lines: &[&str]) {
    let dir = home
        .join(".codex")
        .join("sessions")
        .join("2026")
        .join("04")
        .join("15");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-{session_id}.jsonl"));
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        r#"{{"type":"session_meta","timestamp":"2026-04-15T10:00:00Z","payload":{{"id":"{sid}","cwd":"{cwd}"}}}}"#,
        sid = session_id,
        cwd = cwd
    ));
    lines.extend(extra_lines.iter().map(|s| s.to_string()));
    std::fs::write(path, lines.join("\n")).unwrap();
}

/// Spawn `aivo mcp-serve --cwd <cwd>` with HOME overridden to `home`.
/// Returns (child, send_line, read_line).
fn spawn_server(
    home: &Path,
    cwd: &Path,
) -> (
    std::process::Child,
    impl FnMut(&str),
    impl FnMut() -> Option<Value>,
) {
    let exe = aivo_exe();
    let mut child = Command::new(&exe)
        .arg("mcp-serve")
        .arg("--cwd")
        .arg(cwd)
        .env("HOME", home)
        .env("AIVO_TEST_FAST_CRYPTO_OK", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn aivo mcp-serve");

    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let send = move |line: &str| {
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    };
    let recv = move || -> Option<Value> {
        let mut buf = String::new();
        match reader.read_line(&mut buf) {
            Ok(0) => None,
            Ok(_) => serde_json::from_str(buf.trim()).ok(),
            Err(_) => None,
        }
    };
    (child, send, recv)
}

#[test]
#[cfg_attr(
    windows,
    ignore = "encode_claude_dir and seed helpers assume Unix-style paths; Windows Claude session layout is unresolved"
)]
fn initialize_tools_list_and_get_session_end_to_end() {
    // Skip if the binary isn't available (e.g. `cargo test --no-run`).
    let exe = aivo_exe();
    if !exe.exists() {
        eprintln!("skipping: aivo binary not found at {}", exe.display());
        return;
    }

    let home_dir = tempfile::TempDir::new().unwrap();
    let home = home_dir.path();
    let project_dir = tempfile::TempDir::new().unwrap();
    let project_root = std::fs::canonicalize(project_dir.path()).unwrap();

    // Seed Claude + Codex fixtures scoped to this project.
    seed_claude(
        home,
        &project_root.to_string_lossy(),
        "sid-abc-1234",
        &[
            r#"{"type":"user","sessionId":"sid-abc-1234","isSidechain":false,"timestamp":"2026-04-15T10:00:00Z","message":{"content":"Please review my pagination helper in handlers/users.go."}}"#,
            r#"{"type":"assistant","sessionId":"sid-abc-1234","isSidechain":false,"timestamp":"2026-04-15T10:01:00Z","message":{"content":[{"type":"text","text":"Found two bugs: (1) empty cursor returns 500, (2) limit > 1000 is not clamped."}]}}"#,
        ],
    );
    seed_codex(
        home,
        &project_root.to_string_lossy(),
        "codex-xyz-9999",
        &[
            r#"{"type":"response_item","timestamp":"2026-04-15T10:05:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Please review the pagination patch in handlers/users.go for correctness."}]}}"#,
            r#"{"type":"response_item","timestamp":"2026-04-15T10:06:00Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"LGTM overall but there is one nit: empty cursor still 500s."}]}}"#,
        ],
    );

    let (mut child, mut send, mut recv) = spawn_server(home, &project_root);

    // 1. initialize
    send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
    let resp = recv().expect("initialize response");
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["serverInfo"]["name"], "aivo");

    // notifications/initialized — no response expected; do not wait.
    send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);

    // 2. tools/list
    send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
    let resp = recv().expect("tools/list response");
    let tools = resp["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);

    // 3. list_sessions
    send(
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_sessions","arguments":{}}}"#,
    );
    let resp = recv().expect("list_sessions response");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    let sessions = parsed["sessions"].as_array().unwrap();
    assert!(
        sessions.iter().any(|s| s["cli"] == "claude"),
        "expected a claude session, got {:?}",
        sessions
    );
    assert!(
        sessions.iter().any(|s| s["cli"] == "codex"),
        "expected a codex session, got {:?}",
        sessions
    );

    // 4. get_session for codex (what the "fix the issue codex just found" flow needs).
    send(
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_session","arguments":{"cli":"codex"}}}"#,
    );
    let resp = recv().expect("get_session codex response");
    // No isError — it should return a transcript text block.
    assert!(resp["result"]["isError"].is_null() || resp["result"]["isError"] == false);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["cli"], "codex");
    assert_eq!(parsed["session_id"], "codex-xyz-9999");
    assert!(parsed["turns"].as_array().unwrap().len() >= 2);
    let assistant_text = parsed["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["role"] == "assistant")
        .and_then(|t| t["text"].as_str())
        .unwrap();
    assert!(
        assistant_text.contains("empty cursor"),
        "expected assistant to mention 'empty cursor', got: {assistant_text}"
    );

    // 5. get_session for a CLI with no sessions → tool error, not protocol error.
    send(
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"get_session","arguments":{"cli":"claude","session_id":"nonexistent-prefix"}}}"#,
    );
    let resp = recv().expect("get_session miss response");
    assert_eq!(resp["result"]["isError"], true);

    // 6. Unknown method → JSON-RPC -32601.
    send(r#"{"jsonrpc":"2.0","id":6,"method":"something/weird"}"#);
    let resp = recv().expect("unknown method response");
    assert_eq!(resp["error"]["code"], -32601);

    // 7. Close stdin — server should exit cleanly.
    drop(send);
    drop(recv);
    let status = child.wait().expect("wait for mcp-serve");
    assert!(
        status.success(),
        "server should exit 0 on EOF, got {status}"
    );
}

#[test]
#[cfg_attr(
    windows,
    ignore = "encode_claude_dir and seed helpers assume Unix-style paths; Windows Claude session layout is unresolved"
)]
fn mcp_serve_list_sessions_filters_by_cli() {
    let exe = aivo_exe();
    if !exe.exists() {
        return;
    }
    let home_dir = tempfile::TempDir::new().unwrap();
    let home = home_dir.path();
    let project_dir = tempfile::TempDir::new().unwrap();
    let project_root = std::fs::canonicalize(project_dir.path()).unwrap();

    seed_claude(
        home,
        &project_root.to_string_lossy(),
        "sid-only-claude",
        &[
            r#"{"type":"user","sessionId":"sid-only-claude","isSidechain":false,"message":{"content":"Hi there, this is a substantive turn."}}"#,
            r#"{"type":"assistant","sessionId":"sid-only-claude","isSidechain":false,"message":{"content":[{"type":"text","text":"Understood — ready to help."}]}}"#,
        ],
    );
    seed_codex(
        home,
        &project_root.to_string_lossy(),
        "codex-sess",
        &[
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Could you please review the user-facing pagination helper for correctness?"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done — looks good, no blockers found. Shipping recommended."}]}}"#,
        ],
    );

    let (mut child, mut send, mut recv) = spawn_server(home, &project_root);
    send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
    let _ = recv();

    send(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_sessions","arguments":{"cli":"codex"}}}"#,
    );
    let resp = recv().expect("list_sessions cli=codex");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    let sessions = parsed["sessions"].as_array().unwrap();
    for s in sessions {
        assert_eq!(s["cli"], "codex", "filter should return only codex");
    }
    assert!(!sessions.is_empty(), "should return the codex session");

    let _ = child.kill();
}

#[test]
fn mcp_serve_exits_on_parse_error_then_continues() {
    let exe = aivo_exe();
    if !exe.exists() {
        return;
    }
    let home_dir = tempfile::TempDir::new().unwrap();
    let project_dir = tempfile::TempDir::new().unwrap();
    let project_root = std::fs::canonicalize(project_dir.path()).unwrap();

    let (mut child, mut send, mut recv) = spawn_server(home_dir.path(), &project_root);

    // Garbage line → -32700 Parse error, server stays up.
    send("{not valid json");
    let resp = recv().expect("should send parse-error response");
    assert_eq!(resp["error"]["code"], -32700);

    // Valid request still works.
    send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
    let resp = recv().expect("initialize should work after parse error");
    assert_eq!(resp["id"], 1);

    // Silence Drop-before-wait warnings on CI.
    let _ = child.kill();
}

/// Simulates the 3-window scenario: 2 Claude sessions + 1 Codex in the
/// same cwd. Verifies the same-CLI self-reference workaround —
/// `get_session(cli="claude", exclude_session_ids=[my_id])` returns the
/// *other* Claude session, not the caller's own.
#[test]
#[cfg_attr(
    windows,
    ignore = "encode_claude_dir and seed helpers assume Unix-style paths; Windows Claude session layout is unresolved"
)]
fn three_window_same_cli_peer_lookup_via_exclude_session_ids() {
    let exe = aivo_exe();
    if !exe.exists() {
        return;
    }
    let home_dir = tempfile::TempDir::new().unwrap();
    let home = home_dir.path();
    let project_dir = tempfile::TempDir::new().unwrap();
    let project_root = std::fs::canonicalize(project_dir.path()).unwrap();
    let cwd_str = project_root.to_string_lossy().to_string();

    // Two Claude sessions. Write peer first so it has the OLDER mtime; the
    // caller is written last and is therefore the "newest", matching the
    // real-world scenario where the caller's session file is being
    // actively appended to.
    seed_claude(
        home,
        &cwd_str,
        "sid-peer-BBBB",
        &[
            r#"{"type":"user","sessionId":"sid-peer-BBBB","isSidechain":false,"timestamp":"2026-04-15T10:00:00Z","message":{"content":"This is the PEER session doing unrelated work on parser cleanup."}}"#,
            r#"{"type":"assistant","sessionId":"sid-peer-BBBB","isSidechain":false,"timestamp":"2026-04-15T10:00:30Z","message":{"content":[{"type":"text","text":"I'll refactor the parser function signatures."}]}}"#,
        ],
    );
    // Small sleep so mtimes differ on fast filesystems.
    std::thread::sleep(std::time::Duration::from_millis(20));
    seed_claude(
        home,
        &cwd_str,
        "sid-caller-AAAA",
        &[
            r#"{"type":"user","sessionId":"sid-caller-AAAA","isSidechain":false,"timestamp":"2026-04-15T11:00:00Z","message":{"content":"This is the CALLER session asking about the other window."}}"#,
            r#"{"type":"assistant","sessionId":"sid-caller-AAAA","isSidechain":false,"timestamp":"2026-04-15T11:00:30Z","message":{"content":[{"type":"text","text":"OK, let me check what the other claude is doing."}]}}"#,
        ],
    );
    // Codex session so list_sessions also has cross-CLI content.
    seed_codex(
        home,
        &cwd_str,
        "codex-zzz",
        &[
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Please review the new pagination patch thoroughly end-to-end."}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Found two findings: empty cursor still 500s; limit is not clamped."}]}}"#,
        ],
    );

    let (mut child, mut send, mut recv) = spawn_server(home, &project_root);

    send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
    let _ = recv().expect("initialize");

    // Default behavior: get_session(cli="claude") — returns the NEWEST, which
    // is the caller's own session. This is the "self-trap" the schema doc warns about.
    send(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_session","arguments":{"cli":"claude"}}}"#,
    );
    let resp = recv().expect("get_session without exclude");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(
        parsed["session_id"], "sid-caller-AAAA",
        "without exclude, newest (caller) wins — confirms the trap"
    );

    // Workaround: pass the caller's own id in exclude_session_ids → get the peer.
    send(
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_session","arguments":{"cli":"claude","exclude_session_ids":["sid-caller-AAAA"]}}}"#,
    );
    let resp = recv().expect("get_session with exclude");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(
        parsed["session_id"], "sid-peer-BBBB",
        "exclude should skip the caller and return the peer"
    );
    // Verify we got the peer's actual turns.
    let joined = parsed["turns"].to_string();
    assert!(
        joined.contains("parser"),
        "should return peer's content (parser cleanup), got: {joined}"
    );

    // Excluding both claude sessions → friendly tool error.
    send(
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_session","arguments":{"cli":"claude","exclude_session_ids":["sid-caller-AAAA","sid-peer-BBBB"]}}}"#,
    );
    let resp = recv().expect("get_session both excluded");
    assert_eq!(resp["result"]["isError"], true);
    let err_text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        err_text.contains("excluding"),
        "error should mention the exclusion, got: {err_text}"
    );

    // Prefix-match exclude: `sid-caller` prefix excludes the caller.
    send(
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"get_session","arguments":{"cli":"claude","exclude_session_ids":["sid-caller"]}}}"#,
    );
    let resp = recv().expect("get_session prefix exclude");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(
        parsed["session_id"], "sid-peer-BBBB",
        "prefix exclude should also skip the caller"
    );

    // Cross-CLI still trivially works — no exclude needed.
    send(
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"get_session","arguments":{"cli":"codex"}}}"#,
    );
    let resp = recv().expect("get_session codex");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["cli"], "codex");
    assert_eq!(parsed["session_id"], "codex-zzz");

    let _ = child.kill();
}

/// Writes a nickname registry file into the shared directory for the given cwd.
/// Registry dir: `home/.config/aivo/share/<cwd.replace('/', '-')>/`
fn write_registry_entry(
    home: &Path,
    cwd: &str,
    nickname: &str,
    cli: &str,
    pid: u32,
    started_at: &str,
) {
    let encoded = cwd.replace('/', "-");
    let dir = home.join(".config/aivo/share").join(encoded);
    std::fs::create_dir_all(&dir).unwrap();
    let entry = json!({
        "nickname": nickname,
        "cli": cli,
        "pid": pid,
        "started_at": started_at,
    });
    let path = dir.join(format!("{nickname}.json"));
    std::fs::write(path, serde_json::to_string_pretty(&entry).unwrap()).unwrap();
}

/// 3-window nickname scenario: reviewer (claude), architect (claude), coder (codex).
/// Verifies nickname-based peer lookup works end-to-end via the MCP protocol.
#[test]
#[cfg_attr(
    windows,
    ignore = "encode_claude_dir and seed helpers assume Unix-style paths; Windows Claude session layout is unresolved"
)]
fn nickname_based_peer_lookup() {
    let exe = aivo_exe();
    if !exe.exists() {
        eprintln!("skipping: aivo binary not found at {}", exe.display());
        return;
    }

    let home_dir = tempfile::TempDir::new().unwrap();
    let home = home_dir.path();
    let project_dir = tempfile::TempDir::new().unwrap();
    let project_root = std::fs::canonicalize(project_dir.path()).unwrap();
    let cwd_str = project_root.to_string_lossy().to_string();
    let pid = std::process::id();

    // -- Seed sessions --
    // Reviewer's claude session (registered at 09:59, session timestamps at 10:00)
    seed_claude(
        home,
        &cwd_str,
        "sid-reviewer-001",
        &[
            r#"{"type":"user","sessionId":"sid-reviewer-001","isSidechain":false,"timestamp":"2026-04-15T10:00:00Z","message":{"content":"Please review the authentication middleware for security issues and edge cases."}}"#,
            r#"{"type":"assistant","sessionId":"sid-reviewer-001","isSidechain":false,"timestamp":"2026-04-15T10:00:30Z","message":{"content":[{"type":"text","text":"Found three issues in the auth middleware: token expiry is not checked, CORS headers are missing, and the rate limiter is disabled."}]}}"#,
        ],
    );

    // Small sleep so mtimes differ.
    std::thread::sleep(std::time::Duration::from_millis(20));

    // Architect's claude session (registered at 10:29, session timestamps at 10:30)
    seed_claude(
        home,
        &cwd_str,
        "sid-architect-002",
        &[
            r#"{"type":"user","sessionId":"sid-architect-002","isSidechain":false,"timestamp":"2026-04-15T10:30:00Z","message":{"content":"Design the database schema for the new notification service including tables and indexes."}}"#,
            r#"{"type":"assistant","sessionId":"sid-architect-002","isSidechain":false,"timestamp":"2026-04-15T10:30:30Z","message":{"content":[{"type":"text","text":"Here is the schema: notifications table with id, user_id, type, payload, read_at, created_at columns and a composite index on (user_id, read_at)."}]}}"#,
        ],
    );

    // Coder's codex session (timestamps at 10:00)
    seed_codex(
        home,
        &cwd_str,
        "codex-coder-003",
        &[
            r#"{"type":"response_item","timestamp":"2026-04-15T10:00:30Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Implement the pagination helper function in handlers/users.go with cursor-based pagination."}]}}"#,
            r#"{"type":"response_item","timestamp":"2026-04-15T10:01:00Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done — implemented cursor-based pagination with proper empty-cursor handling and limit clamping to 1000."}]}}"#,
        ],
    );

    // -- Write nickname registry entries --
    // Use current PID so liveness check passes.
    write_registry_entry(
        home,
        &cwd_str,
        "reviewer",
        "claude",
        pid,
        "2026-04-15T09:59:00Z",
    );
    write_registry_entry(
        home,
        &cwd_str,
        "architect",
        "claude",
        pid,
        "2026-04-15T10:29:00Z",
    );
    write_registry_entry(
        home,
        &cwd_str,
        "coder",
        "codex",
        pid,
        "2026-04-15T09:59:00Z",
    );

    // -- Spawn server (no --nickname / --caller-cli — registry is pre-seeded) --
    let (mut child, mut send, mut recv) = spawn_server(home, &project_root);

    // 1. Initialize handshake
    send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
    let resp = recv().expect("initialize response");
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["serverInfo"]["name"], "aivo");

    send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);

    // 2. list_sessions — verify nickname annotations
    send(
        r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"list_sessions","arguments":{}}}"#,
    );
    let resp = recv().expect("list_sessions response");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    let sessions = parsed["sessions"].as_array().unwrap();
    assert!(
        sessions.len() >= 3,
        "expected at least 3 sessions, got {}",
        sessions.len()
    );
    // At least one session should have a nickname annotation.
    let has_nickname = sessions.iter().any(|s| !s["nickname"].is_null());
    assert!(
        has_nickname,
        "expected at least one session with a nickname annotation, got: {sessions:?}"
    );

    // 3. get_session(nickname="coder") — should return the codex transcript
    send(
        r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"get_session","arguments":{"nickname":"coder"}}}"#,
    );
    let resp = recv().expect("get_session nickname=coder");
    assert!(
        resp["result"]["isError"].is_null() || resp["result"]["isError"] == false,
        "get_session(coder) should not be an error: {resp:?}"
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(
        parsed["cli"], "codex",
        "coder nickname should resolve to codex"
    );
    let turns_str = parsed["turns"].to_string();
    assert!(
        turns_str.contains("pagination"),
        "coder transcript should mention pagination, got: {turns_str}"
    );

    // 4. get_session(nickname="architect") — should return the architect's claude
    //    transcript (not reviewer's), distinguishable by content
    send(
        r#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"get_session","arguments":{"nickname":"architect"}}}"#,
    );
    let resp = recv().expect("get_session nickname=architect");
    assert!(
        resp["result"]["isError"].is_null() || resp["result"]["isError"] == false,
        "get_session(architect) should not be an error: {resp:?}"
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(
        parsed["cli"], "claude",
        "architect nickname should resolve to claude"
    );
    let turns_str = parsed["turns"].to_string();
    assert!(
        turns_str.contains("schema") || turns_str.contains("notification"),
        "architect transcript should mention schema/notification (not auth middleware), got: {turns_str}"
    );
    // Verify it's NOT the reviewer's content.
    assert!(
        !turns_str.contains("auth middleware"),
        "architect transcript should NOT contain reviewer's auth middleware content, got: {turns_str}"
    );

    // 5. get_session(nickname="nonexistent") — should return isError:true
    send(
        r#"{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"get_session","arguments":{"nickname":"nonexistent"}}}"#,
    );
    let resp = recv().expect("get_session nickname=nonexistent");
    assert_eq!(
        resp["result"]["isError"], true,
        "nonexistent nickname should return isError:true, got: {resp:?}"
    );
    let err_text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        err_text.contains("nonexistent"),
        "error message should mention the nickname, got: {err_text}"
    );

    // 6. Clean up
    let _ = child.kill();
}

/// Quieten `unused` lint on the helper when it's not wired into a particular test.
#[allow(dead_code)]
fn _compile_check(_: &Path) {
    let _ = json!({});
}
