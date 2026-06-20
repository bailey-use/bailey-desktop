//! Best-effort OS sandbox for the agent's `run_bash` tool. The goal is narrow:
//! confine a shell command's file WRITES to the workspace (cwd) plus temp dirs
//! and the common dev-tool caches, while leaving reads, process exec, and the
//! network open — the agent is still expected to fetch live data and inspect the
//! system (see the engine's system prompt). This is the safety counterpart to
//! the heuristic destructive-command gate: the heuristic catches `rm -rf` inside
//! the workspace (which the sandbox allows), the sandbox catches a stray write
//! to `/etc` or `~/.ssh` (which the heuristic misses).
//!
//! Backends:
//! - **macOS** via `sandbox-exec` (Apple seatbelt) — an external wrapper binary
//!   spawned around the shell.
//! - **Linux** via Landlock (kernel 5.13+). Landlock has no external wrapper —
//!   the ruleset must be installed by syscall *in the process* before running
//!   the shell — so `wrap_shell` re-executes the aivo binary as a hidden
//!   `__agent-sandbox` subcommand (dispatched in `run::run`) which installs the
//!   ruleset and then spawns the shell (Landlock confinement is inherited by
//!   children). Degrades to no confinement on kernels without Landlock.
//! - **Windows**: no-op for now. Windows has no path-allowlist write-confinement
//!   primitive comparable to seatbelt/Landlock — restricted tokens / integrity
//!   levels gate by ACL not by path (and would break ordinary writes), Job
//!   Objects govern CPU/memory not the filesystem, and AppContainer (the nearest
//!   fit) is heavyweight and brittle for an arbitrary `cmd /C`. The heuristic
//!   destructive-command gate still applies. AppContainer is the eventual path
//!   if pursued.
//!
//! Default-on where supported; opt out everywhere with `AIVO_AGENT_NO_SANDBOX=1`.

use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

/// The program + args `run_bash` should actually spawn for a given shell command
/// — either a sandbox wrapper around the shell, or the bare shell when no
/// sandbox applies.
pub struct ShellInvocation {
    pub program: String,
    pub args: Vec<String>,
}

/// Whether a write-confining sandbox is active for this process. Used by
/// `run_bash` to add a hint when a command likely failed because the sandbox
/// blocked a write.
pub fn active() -> bool {
    if disabled() {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        Path::new(SANDBOX_EXEC).exists()
    }
    #[cfg(target_os = "linux")]
    {
        landlock_supported()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

/// Opt out via `AIVO_AGENT_NO_SANDBOX` (any value other than empty/`0`).
fn disabled() -> bool {
    std::env::var("AIVO_AGENT_NO_SANDBOX")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Build the spawn target for `command`, applying the OS sandbox when available.
/// `cwd` is the workspace root writes are confined to.
pub fn wrap_shell(command: &str, cwd: &Path) -> ShellInvocation {
    #[cfg(target_os = "macos")]
    if active() {
        return ShellInvocation {
            program: SANDBOX_EXEC.to_string(),
            args: vec![
                "-p".to_string(),
                macos_profile(cwd),
                "sh".to_string(),
                "-c".to_string(),
                command.to_string(),
            ],
        };
    }

    // Linux: re-execute ourselves as the hidden `__agent-sandbox` subcommand,
    // which installs a Landlock ruleset (confining writes to `cwd` + caches) and
    // then runs the shell. Falls through to the bare shell if `current_exe`
    // can't be resolved.
    #[cfg(target_os = "linux")]
    if active()
        && let Ok(exe) = std::env::current_exe()
    {
        return ShellInvocation {
            program: exe.to_string_lossy().into_owned(),
            args: vec![
                "__agent-sandbox".to_string(),
                "--workspace".to_string(),
                cwd.to_string_lossy().into_owned(),
                "--".to_string(),
                "sh".to_string(),
                "-c".to_string(),
                command.to_string(),
            ],
        };
    }

    bare_shell(command)
}

/// The plain shell invocation with no sandbox wrapper. Used by `wrap_shell` when
/// no sandbox applies, and by `run_bash`'s escalation path when the user
/// approves re-running a sandbox-blocked command outside the workspace.
pub fn bare_shell(command: &str) -> ShellInvocation {
    let (program, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    ShellInvocation {
        program: program.to_string(),
        args: vec![flag.to_string(), command.to_string()],
    }
}

// ---------------------------------------------------------------------------
// macOS (seatbelt) backend
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// A seatbelt (SBPL) profile: allow everything, then deny all file writes, then
/// re-allow writes to the workspace, temp dirs, dev-tool caches, and package
/// prefixes. Last matching rule wins, so the re-allow list carves holes in the
/// blanket write deny. Reads / exec / network stay open from `(allow default)`.
#[cfg(target_os = "macos")]
fn macos_profile(cwd: &Path) -> String {
    let mut writable: Vec<String> = vec![
        "/tmp".into(),
        "/private/tmp".into(),
        "/var/folders".into(),
        "/private/var/folders".into(),
        "/dev".into(),
        // Package-manager prefixes so `brew`, etc. keep working.
        "/usr/local".into(),
        "/opt/homebrew".into(),
    ];
    // The workspace and its real (symlink-resolved) path — seatbelt matches the
    // resolved path of the target, so a symlinked cwd needs both forms.
    writable.push(cwd.to_string_lossy().into_owned());
    if let Ok(canon) = cwd.canonicalize() {
        writable.push(canon.to_string_lossy().into_owned());
    }
    if let Some(tmp) = std::env::var_os("TMPDIR") {
        writable.push(tmp.to_string_lossy().into_owned());
    }
    if let Some(home) = crate::services::system_env::home_dir() {
        // Dev-tool caches, deliberately NOT `~/.config`: that holds aivo's own
        // encrypted key store (`~/.config/aivo`) and every app's config, which
        // the agent shouldn't be able to silently rewrite. A command that
        // genuinely needs to write there hits the escalation prompt instead.
        for sub in [
            ".cache",
            ".cargo",
            ".rustup",
            ".npm",
            ".gradle",
            ".m2",
            ".cocoapods",
            "Library/Caches",
        ] {
            writable.push(home.join(sub).to_string_lossy().into_owned());
        }
    }

    let mut profile =
        String::from("(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*\n");
    for path in writable {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            continue;
        }
        profile.push_str(&format!("    (subpath \"{}\")\n", sbpl_escape(trimmed)));
    }
    profile.push_str(")\n");
    profile
}

/// Escape a path for an SBPL double-quoted string literal.
#[cfg(target_os = "macos")]
fn sbpl_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ---------------------------------------------------------------------------
// Linux (Landlock) backend
// ---------------------------------------------------------------------------

/// The same write-allowlist as the macOS profile, but with Linux paths and
/// filtered to entries that actually exist (Landlock errors on a rule for a
/// missing path). Pure function — unit-testable without the kernel feature.
#[cfg(target_os = "linux")]
fn linux_writable_paths(cwd: &Path) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![
        PathBuf::from("/tmp"),
        PathBuf::from("/var/tmp"),
        PathBuf::from("/dev"),
        // Package-manager prefix.
        PathBuf::from("/usr/local"),
    ];
    candidates.push(cwd.to_path_buf());
    if let Ok(canon) = cwd.canonicalize() {
        candidates.push(canon);
    }
    if let Some(tmp) = std::env::var_os("TMPDIR") {
        candidates.push(PathBuf::from(tmp));
    }
    // Per-user runtime dir (usually /run/user/<uid>).
    if let Some(run) = std::env::var_os("XDG_RUNTIME_DIR") {
        candidates.push(PathBuf::from(run));
    }
    if let Some(home) = crate::services::system_env::home_dir() {
        // Dev-tool caches, deliberately NOT `~/.config`: that holds aivo's own
        // encrypted key store (`~/.config/aivo`) and every app's config, which
        // the agent shouldn't be able to silently rewrite. A command that
        // genuinely needs to write there hits the escalation prompt instead.
        for sub in [
            ".cache",
            ".cargo",
            ".rustup",
            ".npm",
            ".gradle",
            ".m2",
            ".local/share",
        ] {
            candidates.push(home.join(sub));
        }
    }
    // Landlock add_rule fails on a non-existent path; only keep present ones.
    candidates.retain(|p| p.exists());
    candidates
}

/// Whether the running kernel supports Landlock. Probes by *creating* a ruleset
/// fd with a hard compatibility requirement (so an unsupported kernel reports
/// failure rather than a silent best-effort no-op); creating the fd does NOT
/// restrict this process — only `restrict_self` would — so the probe is safe.
/// Cached: the kernel capability can't change within a process lifetime.
#[cfg(target_os = "linux")]
fn landlock_supported() -> bool {
    use std::sync::OnceLock;
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        use landlock::{ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr};
        Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI::V1))
            .and_then(|r| r.create())
            .is_ok()
    })
}

/// Install a Landlock ruleset on the current process confining file writes to
/// `cwd` + the cache allowlist. Best-effort: the highest ABI the kernel supports
/// is negotiated and unsupported rights are dropped; on any failure it returns
/// `false` (caller degrades to running unconfined). Only WRITE accesses are
/// handled, so reads and exec stay open; network is never restricted.
#[cfg(target_os = "linux")]
fn apply_landlock(cwd: &Path) -> bool {
    use landlock::{
        ABI, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus,
    };
    let abi = ABI::V1;
    let write = AccessFs::from_write(abi);
    let created = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(write)
        .and_then(|r| r.create());
    let mut created = match created {
        Ok(c) => c,
        Err(_) => return false,
    };
    for path in linux_writable_paths(cwd) {
        let Ok(fd) = PathFd::new(&path) else {
            continue; // skip a path that vanished between the exists() check and now
        };
        created = match created.add_rule(PathBeneath::new(fd, write)) {
            Ok(c) => c,
            Err(_) => return false,
        };
    }
    matches!(
        created.restrict_self(),
        Ok(status) if !matches!(status.ruleset, RulesetStatus::NotEnforced)
    )
}

/// Split the `__agent-sandbox` argv into the workspace path and the shell argv
/// after `--`. Factored out for unit testing (the entry point below diverges).
/// `raw_args` is the full process argv (`[exe, "__agent-sandbox", …]`).
#[cfg(target_os = "linux")]
fn parse_sandbox_child_args(raw_args: &[String]) -> (Option<String>, Vec<String>) {
    let mut workspace = None;
    let mut rest = Vec::new();
    let mut i = 2; // skip exe + subcommand
    while i < raw_args.len() {
        match raw_args[i].as_str() {
            "--workspace" => {
                workspace = raw_args.get(i + 1).cloned();
                i += 2;
            }
            "--" => {
                rest = raw_args[i + 1..].to_vec();
                break;
            }
            _ => i += 1,
        }
    }
    (workspace, rest)
}

/// Entry point for the hidden `aivo __agent-sandbox` re-exec (dispatched in
/// `run::run` before clap). Installs the Landlock ruleset (best-effort, degrades
/// silently) and runs the shell as a child — Landlock confinement is inherited,
/// so the child shell is confined — then exits with the shell's status. Never
/// returns.
#[cfg(target_os = "linux")]
pub fn run_sandbox_child(raw_args: &[String]) -> ! {
    let (workspace, rest) = parse_sandbox_child_args(raw_args);
    if rest.is_empty() {
        eprintln!("aivo: __agent-sandbox: no command after `--`");
        std::process::exit(127);
    }
    let cwd = workspace.unwrap_or_else(|| ".".to_string());
    // Best-effort confinement; if Landlock is unavailable we still run the shell.
    let _ = apply_landlock(Path::new(&cwd));
    let status = std::process::Command::new(&rest[0])
        .args(&rest[1..])
        .current_dir(&cwd)
        .status();
    match status {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("aivo: __agent-sandbox: failed to run {}: {e}", rest[0]);
            std::process::exit(127);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;

    #[test]
    fn profile_confines_writes_to_workspace() {
        let profile = macos_profile(Path::new("/Users/x/proj"));
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("(subpath \"/Users/x/proj\")"));
        // Temp is always writable (macOS $TMPDIR lives under /var/folders).
        assert!(profile.contains("/private/var/folders"));
        // Reads/exec/network are not denied.
        assert!(profile.contains("(allow default)"));
        assert!(!profile.contains("(deny file-read"));
        assert!(!profile.contains("(deny network"));
    }

    #[test]
    fn sbpl_escape_handles_quotes_and_backslashes() {
        assert_eq!(sbpl_escape(r#"/a/b"c\d"#), r#"/a/b\"c\\d"#);
    }

    #[test]
    fn wrap_shell_uses_sandbox_exec_when_active() {
        // Only assert the wrapper shape when the sandbox is actually active in
        // this environment (it can be disabled via env).
        let inv = wrap_shell("echo hi", Path::new("/tmp"));
        if active() {
            assert_eq!(inv.program, SANDBOX_EXEC);
            assert_eq!(inv.args[0], "-p");
            assert_eq!(inv.args[2], "sh");
            assert_eq!(inv.args.last().unwrap(), "echo hi");
        } else {
            assert_eq!(inv.program, "sh");
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;

    #[test]
    fn writable_paths_include_present_workspace_and_temp_only() {
        // /tmp always exists; assert it's present and macOS-only paths aren't.
        let paths = linux_writable_paths(Path::new("/tmp"));
        assert!(paths.iter().any(|p| p == Path::new("/tmp")));
        assert!(!paths.iter().any(|p| p.starts_with("/private")));
        assert!(!paths.iter().any(|p| p == Path::new("/opt/homebrew")));
        // Every returned path exists (the filter held).
        assert!(paths.iter().all(|p| p.exists()));
    }

    #[test]
    fn parse_child_args_extracts_workspace_and_command() {
        let raw: Vec<String> = [
            "aivo",
            "__agent-sandbox",
            "--workspace",
            "/x",
            "--",
            "sh",
            "-c",
            "echo hi",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (ws, rest) = parse_sandbox_child_args(&raw);
        assert_eq!(ws.as_deref(), Some("/x"));
        assert_eq!(rest, vec!["sh", "-c", "echo hi"]);
    }

    #[test]
    fn wrap_shell_reexecs_through_subcommand_when_active() {
        let inv = wrap_shell("echo hi", Path::new("/tmp"));
        if active() && inv.program != "sh" {
            // Re-exec form (current_exe resolved).
            assert_eq!(inv.args[0], "__agent-sandbox");
            assert!(inv.args.iter().any(|a| a == "--workspace"));
            assert!(inv.args.iter().any(|a| a == "--"));
            assert_eq!(inv.args.last().unwrap(), "echo hi");
        } else {
            // Sandbox off (or current_exe failed) → bare shell.
            assert_eq!(inv.program, "sh");
        }
    }
}
