//! Shadow `CODEX_HOME` for launching the native `codex` CLI with aivo's
//! ChatGPT OAuth credentials — without ever touching the user's real
//! `~/.codex/`.
//!
//! Flow (mirrors the Pi `PI_CODING_AGENT_DIR` pattern in
//! `launch_runtime.rs::write_pi_agent_dir`):
//! 1. Create a temp dir `aivo-codex-<random>/`.
//! 2. Write `auth.json` in the native codex `AuthDotJson` schema
//!    (see `openai/codex: codex-rs/login/src/token_data.rs`).
//! 3. Caller sets `CODEX_HOME=<dir>` on the child env and spawns codex.
//! 4. On exit, `read_back` reads the (possibly-rotated) auth.json so the
//!    refreshed tokens can be persisted back into aivo's store.
//! 5. The temp dir is removed.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::services::codex_oauth::CodexOAuthCredential;

/// On-disk shape expected by the native `codex` CLI. Keep the JSON stable
/// across codex versions: extra fields are preserved on read (via
/// `serde_json::Value`) so round-trip doesn't clobber future additions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthDotJson {
    #[serde(rename = "OPENAI_API_KEY", default)]
    pub openai_api_key: Option<String>,
    pub tokens: TokenData,
    #[serde(default)]
    pub last_refresh: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenData {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl AuthDotJson {
    pub fn from_credential(c: &CodexOAuthCredential) -> Self {
        Self {
            openai_api_key: None,
            tokens: TokenData {
                id_token: c.id_token.clone(),
                access_token: c.access_token.clone(),
                refresh_token: c.refresh_token.clone(),
                account_id: c.account_id.clone(),
            },
            last_refresh: Some(c.last_refresh),
        }
    }

    /// Projects the on-disk auth.json back to an aivo credential, preferring
    /// the disk values for the three tokens and `account_id`, and preserving
    /// the passed-in `email` + `expires_at` (codex doesn't track either
    /// separately).
    pub fn into_credential(
        self,
        email: Option<String>,
        fallback_expires_at: DateTime<Utc>,
    ) -> CodexOAuthCredential {
        let last_refresh = self.last_refresh.unwrap_or_else(Utc::now);
        CodexOAuthCredential {
            id_token: self.tokens.id_token,
            access_token: self.tokens.access_token,
            refresh_token: self.tokens.refresh_token,
            account_id: self.tokens.account_id,
            email,
            // codex doesn't persist `expires_at`; aivo will refresh-on-demand
            // before next launch, so a stale value here is fine.
            expires_at: fallback_expires_at,
            last_refresh,
        }
    }
}

/// Owns a temp dir containing a single `auth.json`. Dropping removes the
/// directory; callers who want to sync refreshed tokens back must call
/// `read_back` before the value is dropped.
pub struct CodexHomeShadow {
    dir: tempfile::TempDir,
}

impl CodexHomeShadow {
    /// Creates the temp dir and writes `auth.json`.
    pub async fn create(creds: &CodexOAuthCredential) -> Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("aivo-codex-")
            .tempdir()
            .context("create CODEX_HOME shadow temp dir")?;

        let auth = AuthDotJson::from_credential(creds);
        let body = serde_json::to_vec_pretty(&auth).context("serialize auth.json")?;
        tokio::fs::write(dir.path().join("auth.json"), body)
            .await
            .context("write shadow auth.json")?;

        Ok(Self { dir })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn auth_path(&self) -> PathBuf {
        self.dir.path().join("auth.json")
    }

    /// Reads the on-disk auth.json back (after codex exits). If the file is
    /// missing or malformed — codex crashed, user killed it, etc. —
    /// returns `Ok(None)` so the caller can keep the pre-launch credential
    /// intact.
    pub async fn read_back(&self) -> Result<Option<AuthDotJson>> {
        let path = self.auth_path();
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(anyhow::Error::new(e).context("read shadow auth.json")),
        };
        match serde_json::from_slice::<AuthDotJson>(&bytes) {
            Ok(v) => Ok(Some(v)),
            Err(_) => Ok(None),
        }
    }
}

/// Returns true if the on-disk tokens differ from `original` in any field
/// codex may have rotated.
pub fn tokens_changed(original: &CodexOAuthCredential, disk: &AuthDotJson) -> bool {
    original.refresh_token != disk.tokens.refresh_token
        || original.access_token != disk.tokens.access_token
        || original.id_token != disk.tokens.id_token
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    fn sample_cred() -> CodexOAuthCredential {
        CodexOAuthCredential {
            id_token: "id".into(),
            access_token: "at".into(),
            refresh_token: "rt".into(),
            account_id: Some("acct_1".into()),
            email: Some("a@b.com".into()),
            expires_at: Utc::now() + ChronoDuration::seconds(3600),
            last_refresh: Utc::now(),
        }
    }

    #[tokio::test]
    async fn roundtrip_preserves_tokens() {
        let c = sample_cred();
        let shadow = CodexHomeShadow::create(&c).await.unwrap();
        let back = shadow.read_back().await.unwrap().unwrap();
        assert_eq!(back.tokens.id_token, c.id_token);
        assert_eq!(back.tokens.access_token, c.access_token);
        assert_eq!(back.tokens.refresh_token, c.refresh_token);
        assert_eq!(back.tokens.account_id, c.account_id);
        assert!(back.openai_api_key.is_none());
    }

    #[tokio::test]
    async fn read_back_handles_missing_file() {
        let c = sample_cred();
        let shadow = CodexHomeShadow::create(&c).await.unwrap();
        tokio::fs::remove_file(shadow.auth_path()).await.unwrap();
        assert!(shadow.read_back().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn read_back_handles_malformed_json() {
        let c = sample_cred();
        let shadow = CodexHomeShadow::create(&c).await.unwrap();
        tokio::fs::write(shadow.auth_path(), b"{not json")
            .await
            .unwrap();
        assert!(shadow.read_back().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn detects_rotated_tokens() {
        let c = sample_cred();
        let shadow = CodexHomeShadow::create(&c).await.unwrap();
        let mut disk = shadow.read_back().await.unwrap().unwrap();
        assert!(!tokens_changed(&c, &disk));
        disk.tokens.refresh_token = "rotated".into();
        assert!(tokens_changed(&c, &disk));
    }

    #[test]
    fn into_credential_preserves_metadata() {
        let c = sample_cred();
        let mut auth = AuthDotJson::from_credential(&c);
        auth.tokens.access_token = "new-at".into();
        let back = auth.into_credential(c.email.clone(), c.expires_at);
        assert_eq!(back.access_token, "new-at");
        assert_eq!(back.email, c.email);
        assert_eq!(back.expires_at, c.expires_at);
    }

    #[tokio::test]
    async fn temp_dir_is_removed_on_drop() {
        let c = sample_cred();
        let path = {
            let shadow = CodexHomeShadow::create(&c).await.unwrap();
            shadow.path().to_path_buf()
        };
        assert!(!path.exists());
    }
}
