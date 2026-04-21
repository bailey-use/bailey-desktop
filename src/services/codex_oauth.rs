//! OpenAI Codex ChatGPT OAuth 2.0 flow (PKCE) for aivo.
//!
//! Provides:
//! - `CodexOAuthCredential`: the stored token bundle (access + refresh + id).
//! - `build_authorize_url`: authorization URL with PKCE + state.
//! - `exchange_code` / `refresh`: HTTP token exchanges.
//! - `interactive_login`: end-to-end browser flow with manual-paste fallback.
//!
//! Multiple accounts are supported: each sign-in produces an independent
//! `CodexOAuthCredential`, persisted as an `ApiKey` with the sentinel
//! `base_url = "codex-oauth"` and the serialized credential in the encrypted
//! `key` slot. The native `codex` CLI is never told about these tokens
//! directly; at launch time aivo projects the selected credential into a
//! shadow `CODEX_HOME` temp dir, which codex reads without touching the
//! user's real `~/.codex/`.

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// OpenAI Codex OAuth application (shared with the native `codex` CLI; see
/// `codex-rs/login/src/auth/manager.rs::CLIENT_ID`).
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// The port must be exactly 1455 because the OAuth application has this
/// redirect URI registered. If it is unavailable the flow falls back to
/// manual URL paste.
pub const CALLBACK_PORT: u16 = 1455;
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

pub const SCOPE: &str = "openid profile email offline_access";

/// Sentinel stored in `ApiKey.base_url` to identify Codex OAuth entries.
/// Mirrors the existing `"copilot"` / `"ollama"` sentinels.
pub const CODEX_OAUTH_SENTINEL: &str = "codex-oauth";

/// Refresh `access_token` this long before its real expiry to avoid
/// mid-flight expirations during launch.
pub const REFRESH_SKEW_SECS: i64 = 120;

/// Tokens persisted per ChatGPT account. Serialized as JSON, then encrypted
/// through the normal `ApiKey.key` pipeline, so the secrets stay AES-GCM
/// encrypted at rest just like a plain API key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodexOAuthCredential {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub last_refresh: DateTime<Utc>,
}

impl CodexOAuthCredential {
    pub fn is_expired(&self, skew_secs: i64) -> bool {
        Utc::now() + ChronoDuration::seconds(skew_secs) >= self.expires_at
    }

    /// Serializes to JSON. The result is passed to `ApiKeyStore` where it
    /// will be AES-GCM encrypted before hitting disk.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("serialize CodexOAuthCredential")
    }

    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).context("parse CodexOAuthCredential JSON")
    }
}

/// PKCE pair for a single authorize flow. `verifier` is never logged or
/// serialized — it lives only in memory for the duration of the flow.
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

impl PkcePair {
    pub fn generate() -> Self {
        // 32 random bytes → 43 URL-safe base64 chars (no padding). RFC 7636
        // requires 43-128 chars of [A-Z a-z 0-9 -._~]; URL_SAFE_NO_PAD uses
        // the "-._~" alphabet subset, which satisfies the spec.
        let mut buf = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf);
        let verifier = URL_SAFE_NO_PAD.encode(buf);
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(digest);
        Self {
            verifier,
            challenge,
        }
    }
}

/// 32-hex-char state (16 random bytes). Matches codex-multi-auth.
pub fn generate_state() -> String {
    let mut buf = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().fold(String::with_capacity(32), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{:02x}", b);
        acc
    })
}

/// Builds the URL the user opens in their browser.
pub fn build_authorize_url(pkce_challenge: &str, state: &str) -> String {
    let encoded_redirect = crate::services::percent_codec::encode(REDIRECT_URI);
    let encoded_scope = crate::services::percent_codec::encode(SCOPE);
    format!(
        "{AUTHORIZE_URL}?response_type=code\
         &client_id={CLIENT_ID}\
         &redirect_uri={encoded_redirect}\
         &scope={encoded_scope}\
         &code_challenge={pkce_challenge}\
         &code_challenge_method=S256\
         &state={state}\
         &id_token_add_organizations=true\
         &codex_cli_simplified_flow=true\
         &originator=codex_cli_rs"
    )
}

/// Raw token endpoint response. Not exposed; fields flow into
/// `CodexOAuthCredential` after we decode the id_token.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: i64,
}

/// Exchanges an authorization code for a full token bundle.
pub async fn exchange_code(code: &str, pkce_verifier: &str) -> Result<CodexOAuthCredential> {
    let client = crate::services::http_utils::router_http_client_with_timeout(30);
    let resp = client
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", pkce_verifier),
            ("redirect_uri", REDIRECT_URI),
        ])
        .send()
        .await
        .context("POST /oauth/token (authorization_code)")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "token exchange failed ({}): {}",
            status.as_u16(),
            redact_oauth_body(&body)
        );
    }

    let tokens: TokenResponse = resp.json().await.context("parse /oauth/token response")?;

    let id_token = tokens
        .id_token
        .ok_or_else(|| anyhow!("token response missing id_token"))?;
    let refresh_token = tokens
        .refresh_token
        .ok_or_else(|| anyhow!("token response missing refresh_token"))?;
    let (email, account_id) = decode_id_token_claims(&id_token);

    let now = Utc::now();
    Ok(CodexOAuthCredential {
        id_token,
        access_token: tokens.access_token,
        refresh_token,
        account_id,
        email,
        expires_at: now + ChronoDuration::seconds(tokens.expires_in),
        last_refresh: now,
    })
}

/// Refreshes `access_token` (and typically rotates `refresh_token`).
/// Mutates `creds` in place. If the server issues a new `refresh_token`,
/// it replaces the old one — the old one is immediately invalidated by
/// OpenAI.
pub async fn refresh(creds: &mut CodexOAuthCredential) -> Result<()> {
    let client = crate::services::http_utils::router_http_client_with_timeout(30);
    let resp = client
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", creds.refresh_token.as_str()),
            ("scope", SCOPE),
        ])
        .send()
        .await
        .context("POST /oauth/token (refresh_token)")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "refresh failed ({}): {}",
            status.as_u16(),
            redact_oauth_body(&body)
        );
    }

    let tokens: TokenResponse = resp.json().await.context("parse refresh response")?;
    let now = Utc::now();
    creds.access_token = tokens.access_token;
    if let Some(new_refresh) = tokens.refresh_token {
        creds.refresh_token = new_refresh;
    }
    if let Some(new_id) = tokens.id_token {
        let (email, account_id) = decode_id_token_claims(&new_id);
        creds.id_token = new_id;
        // id_token claims are stable for a given account, but update in
        // case the user changed their email on the ChatGPT side.
        if email.is_some() {
            creds.email = email;
        }
        if account_id.is_some() {
            creds.account_id = account_id;
        }
    }
    creds.expires_at = now + ChronoDuration::seconds(tokens.expires_in);
    creds.last_refresh = now;
    Ok(())
}

/// Refreshes only if the token is near expiry. Returns `true` if a refresh
/// actually happened (so the caller can persist the new tokens).
pub async fn ensure_fresh(creds: &mut CodexOAuthCredential, skew_secs: i64) -> Result<bool> {
    if creds.is_expired(skew_secs) {
        refresh(creds).await?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Decodes the `payload` segment of a JWT and pulls out the email and
/// ChatGPT account id. Does NOT verify the signature — the id_token comes
/// straight from the token endpoint over TLS, so the JWT claims are trusted
/// by provenance, not cryptography. Mirrors codex-multi-auth's approach.
pub fn decode_id_token_claims(jwt: &str) -> (Option<String>, Option<String>) {
    let mut parts = jwt.split('.');
    let _header = parts.next();
    let payload = match parts.next() {
        Some(p) => p,
        None => return (None, None),
    };
    // JWT uses base64url without padding.
    let decoded = match URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| STANDARD_NO_PAD.decode(payload))
    {
        Ok(bytes) => bytes,
        Err(_) => return (None, None),
    };
    let value: serde_json::Value = match serde_json::from_slice(&decoded) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let email = value
        .get("email")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    // OpenAI embeds the chatgpt_account_id under a namespaced claim. Try
    // both common shapes; fall back to top-level for forward compat.
    let account_id = value
        .get("https://api.openai.com/auth")
        .and_then(|v| v.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .or_else(|| value.get("chatgpt_account_id").and_then(|v| v.as_str()))
        .map(|s| s.to_string());
    (email, account_id)
}

/// Redacts common OAuth secrets before logging an upstream response body.
pub fn redact_oauth_body(body: &str) -> String {
    // Cheap but effective: mask the values after known token keys. Advances
    // past each replacement so we never re-match the same occurrence.
    let mut out = body.to_string();
    for key in [
        "access_token",
        "refresh_token",
        "id_token",
        "code",
        "code_verifier",
    ] {
        let needle = format!("\"{}\"", key);
        let mut cursor = 0usize;
        while let Some(rel_idx) = out[cursor..].find(&needle) {
            let idx = cursor + rel_idx;
            let after_key = idx + needle.len();
            let rest = &out[after_key..];
            let Some(colon) = rest.find(':') else { break };
            let Some(open) = rest[colon..].find('"') else {
                cursor = after_key;
                continue;
            };
            let Some(close_rel) = rest[colon + open + 1..].find('"') else {
                cursor = after_key;
                continue;
            };
            let start = after_key + colon + open + 1;
            let end = start + close_rel;
            out.replace_range(start..end, "<redacted>");
            // Skip past the replacement so we don't rescan the same key.
            cursor = start + "<redacted>".len();
        }
    }
    out
}

/// End-to-end sign-in flow:
/// 1. Generate PKCE + state.
/// 2. Bind `127.0.0.1:1455`. If that fails, fall back to manual URL paste.
/// 3. Show the authorize URL, wait for the user to press Enter, then open
///    the browser (or not — the user may prefer to copy-paste).
/// 4. Await the OAuth callback (or a pasted callback URL).
/// 5. Exchange the code for a full credential bundle.
///
/// Prints the URL to stderr regardless — users on headless/CI hosts or
/// with sandboxed browsers can always open it manually.
pub async fn interactive_login() -> Result<CodexOAuthCredential> {
    use crate::services::browser_open;
    use crate::services::codex_oauth_callback::{PortUnavailable, wait_for_callback};
    use std::io::{BufRead, IsTerminal, Write as _};
    use std::time::Duration;

    let pkce = PkcePair::generate();
    let state = generate_state();
    let authorize_url = build_authorize_url(&pkce.challenge, &state);

    eprintln!("Open this URL in your browser to sign in:");
    eprintln!("  {authorize_url}");
    eprintln!();
    let _ = std::io::stderr().flush();

    // Gate the browser launch on user input so we don't steal focus or
    // flash a new window unexpectedly. On non-TTY hosts (CI, `aivo` piped)
    // we skip the prompt and the browser — the user is expected to open
    // the URL themselves.
    if std::io::stdin().is_terminal() {
        eprint!(
            "Press {} to open in browser (or copy manually) ",
            crate::style::cyan("Enter")
        );
        let _ = std::io::stderr().flush();
        let mut buf = String::new();
        let _ = std::io::stdin().lock().read_line(&mut buf);
        let _ = browser_open::open_url(&authorize_url);
    }

    // 5-minute ceiling matches codex-multi-auth's 300 × 100ms poll window.
    let result = wait_for_callback(&state, Duration::from_secs(300)).await;

    let code = match result {
        Ok(cb) => cb.code,
        Err(err) => {
            if err.downcast_ref::<PortUnavailable>().is_some() {
                eprintln!("Port {CALLBACK_PORT} is unavailable. Paste the full callback URL here.");
                manual_paste_prompt()?
            } else {
                return Err(err);
            }
        }
    };

    exchange_code(&code, &pkce.verifier).await
}

fn manual_paste_prompt() -> Result<String> {
    use std::io::{BufRead, Write};

    eprint!("Callback URL: ");
    let _ = std::io::stderr().flush();

    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("read callback URL from stdin")?;
    let line = line.trim();
    if line.is_empty() {
        anyhow::bail!("no callback URL provided");
    }

    // Accept either the full URL or just "code=...&state=..."
    let query = line.split_once('?').map(|(_, q)| q).unwrap_or(line);

    let (code, _state, error) =
        crate::services::codex_oauth_callback::extract_callback_params(query);

    if let Some(err) = error {
        anyhow::bail!("callback URL contained an error: {err}");
    }
    code.ok_or_else(|| anyhow!("callback URL missing `code`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_has_expected_shape() {
        let p = PkcePair::generate();
        // 32 bytes → 43 chars URL_SAFE_NO_PAD
        assert_eq!(p.verifier.len(), 43);
        // SHA-256 → 32 bytes → 43 chars URL_SAFE_NO_PAD
        assert_eq!(p.challenge.len(), 43);
        assert!(
            p.verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
        );
    }

    #[test]
    fn pkce_challenge_matches_verifier() {
        let p = PkcePair::generate();
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, expected);
    }

    #[test]
    fn generate_state_is_32_hex() {
        let s = generate_state();
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn authorize_url_includes_all_params() {
        let url = build_authorize_url("test_challenge", "abc123");
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("response_type=code"));
        assert!(url.contains(&format!("client_id={CLIENT_ID}")));
        assert!(url.contains("code_challenge=test_challenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=abc123"));
        assert!(url.contains("id_token_add_organizations=true"));
        assert!(url.contains("codex_cli_simplified_flow=true"));
        assert!(url.contains("originator=codex_cli_rs"));
        // Redirect URI must be percent-encoded so the ':' and '/' survive.
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
    }

    #[test]
    fn credential_json_roundtrip() {
        let c = CodexOAuthCredential {
            id_token: "eyJ".into(),
            access_token: "at".into(),
            refresh_token: "rt".into(),
            account_id: Some("acct_1".into()),
            email: Some("alice@example.com".into()),
            expires_at: Utc::now(),
            last_refresh: Utc::now(),
        };
        let json = c.to_json().unwrap();
        let back = CodexOAuthCredential::from_json(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn is_expired_respects_skew() {
        let mut c = CodexOAuthCredential {
            id_token: "".into(),
            access_token: "".into(),
            refresh_token: "".into(),
            account_id: None,
            email: None,
            expires_at: Utc::now() + ChronoDuration::seconds(60),
            last_refresh: Utc::now(),
        };
        // 120s skew + 60s remaining → already "expired"
        assert!(c.is_expired(120));
        // 30s skew + 60s remaining → still fresh
        assert!(!c.is_expired(30));
        c.expires_at = Utc::now() - ChronoDuration::seconds(1);
        assert!(c.is_expired(0));
    }

    #[test]
    fn decode_id_token_extracts_claims() {
        // Payload: {"email":"a@b.com","https://api.openai.com/auth":{"chatgpt_account_id":"acct_xyz"}}
        let payload = URL_SAFE_NO_PAD.encode(
            br#"{"email":"a@b.com","https://api.openai.com/auth":{"chatgpt_account_id":"acct_xyz"}}"#,
        );
        let jwt = format!("header.{payload}.sig");
        let (email, account_id) = decode_id_token_claims(&jwt);
        assert_eq!(email.as_deref(), Some("a@b.com"));
        assert_eq!(account_id.as_deref(), Some("acct_xyz"));
    }

    #[test]
    fn decode_id_token_tolerates_malformed() {
        assert_eq!(decode_id_token_claims("not-a-jwt"), (None, None));
        assert_eq!(decode_id_token_claims("a.b"), (None, None));
        assert_eq!(decode_id_token_claims("a..c"), (None, None));
    }

    #[test]
    fn redact_masks_token_values() {
        let body = r#"{"access_token":"sk-real","refresh_token":"rt-real","expires_in":3600}"#;
        let red = redact_oauth_body(body);
        assert!(!red.contains("sk-real"));
        assert!(!red.contains("rt-real"));
        assert!(red.contains("<redacted>"));
        assert!(red.contains("3600"));
    }
}
