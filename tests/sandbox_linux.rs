//! Linux-only integration test for the agent's Landlock write sandbox. Drives the
//! real `aivo __agent-sandbox` re-exec entry (the one `run_bash` spawns) and asserts
//! writes are confined to the workspace.
//!
//! Without Landlock the test skips (older kernel / WSL / container). But CI sets
//! `AIVO_REQUIRE_LANDLOCK=1` on the Linux runner, turning a missing Landlock into a
//! hard failure — so a green build means the sandbox actually enforced, not that a
//! silent no-op regression slipped past a skipped test.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;

/// True when the kernel lists Landlock among its active LSMs. Used to gate the
/// test: without Landlock the sandbox degrades to no confinement by design, so
/// asserting blocking would be wrong.
fn landlock_available() -> bool {
    std::fs::read_to_string("/sys/kernel/security/lsm")
        .map(|s| s.split(',').any(|m| m.trim() == "landlock"))
        .unwrap_or(false)
}

/// Whether CI requires Landlock to be provably enforcing here (set on the Linux runner).
fn require_landlock() -> bool {
    std::env::var("AIVO_REQUIRE_LANDLOCK").as_deref() == Ok("1")
}

fn run_in_sandbox(workspace: &Path, shell_cmd: &str) -> bool {
    Command::new(env!("CARGO_BIN_EXE_aivo"))
        .arg("__agent-sandbox")
        .arg("--workspace")
        .arg(workspace)
        .arg("--")
        .args(["sh", "-c", shell_cmd])
        .status()
        .expect("spawn aivo __agent-sandbox")
        .success()
}

#[test]
fn sandbox_allows_workspace_writes_but_blocks_outside() {
    if !landlock_available() {
        assert!(
            !require_landlock(),
            "AIVO_REQUIRE_LANDLOCK=1 but the kernel lists no Landlock: the agent \
             write-sandbox would run UNCONFINED — runner lost Landlock or the \
             sandbox regressed."
        );
        eprintln!("skipping sandbox enforcement test: kernel lacks Landlock");
        return;
    }
    // Both dirs live directly under $HOME (NOT under /tmp, which the sandbox
    // allowlists) so the "outside" dir is genuinely outside the writable set.
    // The workspace is allowed because it's passed as --workspace.
    let home = std::env::var("HOME").expect("HOME");
    let ws = tempfile::Builder::new()
        .prefix(".aivo-sbtest-ws-")
        .tempdir_in(&home)
        .unwrap();
    let outside = tempfile::Builder::new()
        .prefix(".aivo-sbtest-out-")
        .tempdir_in(&home)
        .unwrap();

    let in_ws = ws.path().join("ok.txt");
    let out_file = outside.path().join("escaped.txt");

    // A write inside the workspace must succeed.
    assert!(
        run_in_sandbox(ws.path(), &format!("echo hi > {}", in_ws.display())),
        "in-workspace write should succeed"
    );
    assert!(in_ws.exists(), "in-workspace file should have been created");

    // A write outside the workspace must be blocked by Landlock (file absent).
    let _ = run_in_sandbox(ws.path(), &format!("echo hi > {}", out_file.display()));
    assert!(
        !out_file.exists(),
        "write outside the workspace must be blocked by the sandbox"
    );
}
