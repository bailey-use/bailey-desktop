//! Local record of the signed-in getaivo.dev account.
//!
//! Written by `aivo login` after the device is linked; read by `aivo info`.
//! This is display metadata only — there is no secret here (the Ed25519 device
//! key remains the credential), so it lives as a plain JSON file at
//! `~/.config/aivo/account.json` (mode 0600), separate from the encrypted
//! `config.json` and untouched by other aivo commands.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::services::atomic_write::{atomic_write_secure, ensure_private_dir};
use crate::services::system_env;

/// The account this device is linked to. `email`/`name` are best-effort —
/// the server may omit them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub linked_at: String,
}

impl Account {
    /// Best label for the account: email, then name, then the opaque user id.
    pub fn display(&self) -> &str {
        self.email
            .as_deref()
            .or(self.name.as_deref())
            .unwrap_or(&self.user_id)
    }
}

fn account_path() -> Option<PathBuf> {
    Some(
        system_env::home_dir()?
            .join(".config")
            .join("aivo")
            .join("account.json"),
    )
}

/// Loads the stored account, or `None` if not logged in / unreadable.
pub fn load() -> Option<Account> {
    load_from(&account_path()?)
}

fn load_from(path: &Path) -> Option<Account> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persists the account record atomically (0600).
pub async fn save(account: &Account) -> Result<()> {
    let path = account_path().ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?;
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent).await?;
    }
    let data = serde_json::to_vec_pretty(account)?;
    atomic_write_secure(&path, data).await
}

/// Removes the stored account. Returns true if a record was present.
pub fn clear() -> bool {
    match account_path() {
        Some(path) => std::fs::remove_file(path).is_ok(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample() -> Account {
        Account {
            user_id: "u1".into(),
            email: Some("a@b.co".into()),
            name: Some("Ann".into()),
            linked_at: "2026-06-25T00:00:00Z".into(),
        }
    }

    #[test]
    fn round_trips_through_json() {
        let a = sample();
        let json = serde_json::to_vec_pretty(&a).unwrap();
        let back: Account = serde_json::from_slice(&json).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn load_from_missing_is_none() {
        let dir = TempDir::new().unwrap();
        assert!(load_from(&dir.path().join("account.json")).is_none());
    }

    #[test]
    fn load_from_reads_written_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("account.json");
        std::fs::write(&path, serde_json::to_vec(&sample()).unwrap()).unwrap();
        assert_eq!(load_from(&path), Some(sample()));
    }

    #[test]
    fn email_and_name_are_optional() {
        let json = br#"{"user_id":"u","linked_at":"t"}"#;
        let a: Account = serde_json::from_slice(json).unwrap();
        assert_eq!(a.user_id, "u");
        assert!(a.email.is_none());
        assert!(a.name.is_none());
        assert_eq!(a.display(), "u");
    }

    #[test]
    fn display_prefers_email_then_name() {
        assert_eq!(sample().display(), "a@b.co");
        let mut a = sample();
        a.email = None;
        assert_eq!(a.display(), "Ann");
    }
}
