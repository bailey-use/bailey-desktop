//! Nickname registry for cross-tool MCP communication.
//!
//! Each running tool writes a small JSON file claiming its nickname into a
//! shared directory scoped by the current working directory. The file is
//! automatically cleaned up on exit via [`RegistryGuard`] (RAII).
//!
//! Other tools read the registry to discover peers by name.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::services::context_ingest::encode_claude_dir;
use crate::services::system_env;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub nickname: String,
    pub cli: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
}

/// Deletes the registration file on drop so stale nicknames don't linger
/// after the tool exits (including crashes that unwind).
#[derive(Debug)]
pub struct RegistryGuard {
    path: PathBuf,
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Registry dir: `~/.config/aivo/share/<encode_claude_dir(cwd)>/`
#[allow(dead_code)]
pub fn registry_dir_for_cwd(cwd: &Path) -> Option<PathBuf> {
    let home = system_env::home_dir()?;
    let cwd_str = cwd.to_string_lossy();
    let encoded = encode_claude_dir(&cwd_str);
    Some(home.join(".config/aivo/share").join(encoded))
}

/// Register a nickname for the current process.
///
/// Creates the registry directory if missing, checks for an existing claim,
/// and writes `<nickname>.json`. Returns a guard that deletes the file on
/// drop.
pub async fn register(nickname: &str, cli: &str, registry_root: &Path) -> Result<RegistryGuard> {
    tokio::fs::create_dir_all(registry_root).await?;

    let file_path = registry_root.join(format!("{nickname}.json"));

    // Check for an existing claim.
    if file_path.exists()
        && let Ok(contents) = tokio::fs::read_to_string(&file_path).await
        && let Ok(existing) = serde_json::from_str::<RegistryEntry>(&contents)
        && system_env::is_pid_alive(existing.pid)
    {
        bail!(
            "nickname '{}' already in use by {} (pid {})",
            nickname,
            existing.cli,
            existing.pid
        );
    }

    let entry = RegistryEntry {
        nickname: nickname.to_string(),
        cli: cli.to_string(),
        pid: std::process::id(),
        started_at: Utc::now(),
    };

    let json = serde_json::to_string_pretty(&entry)?;
    tokio::fs::write(&file_path, json).await?;

    Ok(RegistryGuard { path: file_path })
}

/// Auto-register with a name derived from the CLI tool name. Tries the
/// base name first (`claude`), then appends `-2`, `-3`, … if taken.
/// Returns `(chosen_nickname, guard)`.
pub async fn register_auto(cli: &str, registry_root: &Path) -> Result<(String, RegistryGuard)> {
    tokio::fs::create_dir_all(registry_root).await?;

    // Try the base name first, then numbered suffixes.
    let base = cli.to_string();
    for i in 0u32..100 {
        let candidate = if i == 0 {
            base.clone()
        } else {
            format!("{base}-{}", i + 1)
        };
        match register(&candidate, cli, registry_root).await {
            Ok(guard) => return Ok((candidate, guard)),
            Err(_) => continue,
        }
    }
    bail!("could not find a free nickname for '{cli}' after 100 attempts")
}

/// List all active (live-PID) entries in the registry directory.
///
/// Stale entries (dead PIDs) are pruned (deleted) automatically.
pub async fn list_active(registry_root: &Path) -> Vec<RegistryEntry> {
    let mut entries = Vec::new();

    let mut read_dir = match tokio::fs::read_dir(registry_root).await {
        Ok(rd) => rd,
        Err(_) => return entries,
    };

    while let Ok(Some(dir_entry)) = read_dir.next_entry().await {
        let path = dir_entry.path();
        let is_json = path.extension().is_some_and(|ext| ext == "json");
        if !is_json {
            continue;
        }

        let contents = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(_) => continue,
        };

        let entry: RegistryEntry = match serde_json::from_str(&contents) {
            Ok(e) => e,
            Err(_) => continue,
        };

        if system_env::is_pid_alive(entry.pid) {
            entries.push(entry);
        } else {
            // Prune stale entry.
            let _ = tokio::fs::remove_file(&path).await;
        }
    }

    entries
}

/// Resolve a single nickname to its registry entry, if the process is still alive.
pub async fn resolve_nickname(nickname: &str, registry_root: &Path) -> Option<RegistryEntry> {
    let file_path = registry_root.join(format!("{nickname}.json"));

    let contents = tokio::fs::read_to_string(&file_path).await.ok()?;
    let entry: RegistryEntry = serde_json::from_str(&contents).ok()?;

    if system_env::is_pid_alive(entry.pid) {
        Some(entry)
    } else {
        // Prune stale entry.
        let _ = tokio::fs::remove_file(&file_path).await;
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_read_and_cleanup_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        let guard = register("editor", "claude", &root).await.unwrap();

        // Should appear in list_active.
        let active = list_active(&root).await;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].nickname, "editor");
        assert_eq!(active[0].cli, "claude");

        // resolve_nickname should find it.
        let resolved = resolve_nickname("editor", &root).await;
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().nickname, "editor");

        // Drop the guard — file should be removed.
        drop(guard);

        let active = list_active(&root).await;
        assert!(active.is_empty());

        let resolved = resolve_nickname("editor", &root).await;
        assert!(resolved.is_none());
    }

    #[tokio::test]
    async fn duplicate_nickname_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        let _guard = register("coder", "claude", &root).await.unwrap();

        let result = register("coder", "codex", &root).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("already in use"));
        assert!(err_msg.contains("coder"));
    }

    #[tokio::test]
    async fn stale_pid_is_pruned() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        // Manually write a file with a PID that is (almost certainly) dead.
        let stale_entry = RegistryEntry {
            nickname: "ghost".to_string(),
            cli: "codex".to_string(),
            pid: 999_999_999,
            started_at: Utc::now(),
        };
        let file_path = root.join("ghost.json");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            &file_path,
            serde_json::to_string_pretty(&stale_entry).unwrap(),
        )
        .unwrap();

        // File should exist before pruning.
        assert!(file_path.exists());

        // list_active should prune the stale entry and delete the file.
        let active = list_active(&root).await;
        assert!(active.is_empty());
        assert!(!file_path.exists());
    }
}
