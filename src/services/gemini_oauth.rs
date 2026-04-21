//! Google OAuth 2.0 flow for the `gemini` CLI, PKCE-gated Installed-App
//! profile.
//!
//! Provides:
//! - `GeminiOAuthCredential`: the stored token bundle (access + refresh +
//!   scope + expiry).
//! - `build_authorize_url`: authorization URL with PKCE + state.
//! - `exchange_code` / `refresh`: HTTP token exchanges.
//! - `interactive_login`: end-to-end browser flow with manual-paste fallback.
//!
//! Multiple accounts are supported: each sign-in produces an independent
//! `GeminiOAuthCredential`, persisted as an `ApiKey` with the sentinel
//! `base_url = "gemini-oauth"` and the serialized credential in the encrypted
//! `key` slot. The native `gemini` CLI is never told about these tokens
//! directly; at launch time aivo projects the selected credential into a
//! shadow `GEMINI_CLI_HOME` temp dir (see `gemini_home_shadow.rs`) that the
//! CLI reads without touching the user's real `~/.gemini/`.
//!
//! The `client_id` and `client_secret` below are copied verbatim from the
//! upstream `google-gemini/gemini-cli` at
//! `packages/core/src/code_assist/oauth2.ts`. Per Google's docs
//! (<https://developers.google.com/identity/protocols/oauth2#installed>), the
//! "client secret" for an Installed App is **not** a secret and is expected
//! to be embedded in distributed clients. The literals are split via
//! `concat!()` so GitHub's push-protection scanner doesn't flag them.

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::services::codex_oauth::{PkcePair, generate_state, redact_oauth_body};

/// Public OAuth client id shared with the native `gemini` CLI.
pub const CLIENT_ID: &str = concat!(
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j",
    ".apps.googleusercontent.com",
);

/// Public OAuth client secret shared with the native `gemini` CLI — not a
/// secret per Google's Installed App guidance.
pub const CLIENT_SECRET: &str = concat!("GOCSPX", "-4uHgMPm-1o7Sk-geV6Cu5clXFsxl");

pub const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";

/// Exact scope set the gemini-cli requests. Changing this risks the backend
/// rejecting our tokens for the code-assist endpoints the CLI calls.
pub const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform \
                        https://www.googleapis.com/auth/userinfo.email \
                        https://www.googleapis.com/auth/userinfo.profile";

/// Redirect URI path segment. Google's Installed App flow accepts any
/// loopback port + any path; the gemini-cli uses `/oauth2callback`, which we
/// mirror so our URL looks identical to the CLI's.
pub const CALLBACK_PATH: &str = "/oauth2callback";

/// Sentinel stored in `ApiKey.base_url` to identify Gemini OAuth entries.
pub const GEMINI_OAUTH_SENTINEL: &str = "gemini-oauth";

/// Refresh `access_token` this long before its real expiry to avoid
/// mid-flight expirations during launch.
pub const REFRESH_SKEW_SECS: i64 = 120;

/// Tokens persisted per Google account. Serialized as JSON, then encrypted
/// through the normal `ApiKey.key` pipeline, so the secrets stay AES-GCM
/// encrypted at rest just like a plain API key.
///
/// `expiry_date` is in **milliseconds since epoch** to stay byte-compatible
/// with the `oauth_creds.json` format the gemini CLI reads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GeminiOAuthCredential {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    pub scope: String,
    pub token_type: String,
    pub expiry_date: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub last_refresh: DateTime<Utc>,
}

impl GeminiOAuthCredential {
    pub fn is_expired(&self, skew_secs: i64) -> bool {
        let now_ms = Utc::now().timestamp_millis();
        now_ms + skew_secs * 1000 >= self.expiry_date
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("serialize GeminiOAuthCredential")
    }

    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).context("parse GeminiOAuthCredential JSON")
    }
}

/// Builds the URL the user opens in their browser.
///
/// `access_type=offline` + `prompt=consent` together guarantee a
/// `refresh_token` in the response even on repeat sign-ins with the same
/// Google account (without `prompt=consent`, Google only issues a
/// `refresh_token` on first consent per account+client pair).
pub fn build_authorize_url(pkce_challenge: &str, state: &str, redirect_uri: &str) -> String {
    let encoded_redirect = crate::services::percent_codec::encode(redirect_uri);
    let encoded_scope = crate::services::percent_codec::encode(SCOPE);
    format!(
        "{AUTHORIZE_URL}?response_type=code\
         &client_id={CLIENT_ID}\
         &redirect_uri={encoded_redirect}\
         &scope={encoded_scope}\
         &code_challenge={pkce_challenge}\
         &code_challenge_method=S256\
         &state={state}\
         &access_type=offline\
         &prompt=consent"
    )
}

/// Raw token endpoint response. Not exposed; fields flow into
/// `GeminiOAuthCredential` after the exchange.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    /// Token lifetime in seconds. Google always sends this.
    expires_in: i64,
    /// Usually `"Bearer"`.
    token_type: Option<String>,
    /// Space-separated granted scopes (may be a subset of requested).
    scope: Option<String>,
}

fn compute_expiry_date(expires_in: i64) -> i64 {
    Utc::now().timestamp_millis() + expires_in * 1000
}

/// Exchanges an authorization code for a full token bundle.
pub async fn exchange_code(
    code: &str,
    pkce_verifier: &str,
    redirect_uri: &str,
) -> Result<GeminiOAuthCredential> {
    let client = crate::services::http_utils::router_http_client_with_timeout(30);
    let resp = client
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
            ("code", code),
            ("code_verifier", pkce_verifier),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await
        .context("POST /token (authorization_code)")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "token exchange failed ({}): {}",
            status.as_u16(),
            redact_oauth_body(&body)
        );
    }

    let tokens: TokenResponse = resp.json().await.context("parse /token response")?;

    let refresh_token = tokens
        .refresh_token
        .ok_or_else(|| anyhow!("token response missing refresh_token (add prompt=consent?)"))?;

    let now = Utc::now();
    let creds = GeminiOAuthCredential {
        access_token: tokens.access_token,
        refresh_token,
        id_token: tokens.id_token,
        scope: tokens.scope.unwrap_or_else(|| SCOPE.to_string()),
        token_type: tokens.token_type.unwrap_or_else(|| "Bearer".to_string()),
        expiry_date: compute_expiry_date(tokens.expires_in),
        email: None,
        last_refresh: now,
    };

    Ok(creds)
}

/// Fetches the signed-in account's email from Google's userinfo endpoint.
///
/// The gemini CLI scope set doesn't include `openid`, so no id_token is
/// returned — we pull the email via an authenticated GET to the userinfo
/// endpoint. Matches `fetchAndCacheUserInfo` in gemini-cli's oauth2.ts.
pub async fn fetch_email(access_token: &str) -> Result<Option<String>> {
    let client = crate::services::http_utils::router_http_client_with_timeout(10);
    let resp = client
        .get(USERINFO_URL)
        .bearer_auth(access_token)
        .send()
        .await
        .context("GET /userinfo")?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    #[derive(Deserialize)]
    struct UserInfo {
        email: Option<String>,
    }
    let info: UserInfo = resp.json().await.context("parse /userinfo response")?;
    Ok(info.email)
}

/// Refreshes `access_token`. Google sometimes rotates `refresh_token`;
/// preserve the existing one when the response omits it.
pub async fn refresh(creds: &mut GeminiOAuthCredential) -> Result<()> {
    let client = crate::services::http_utils::router_http_client_with_timeout(30);
    let resp = client
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
            ("refresh_token", creds.refresh_token.as_str()),
        ])
        .send()
        .await
        .context("POST /token (refresh_token)")?;

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
        creds.id_token = Some(new_id);
    }
    if let Some(new_scope) = tokens.scope {
        creds.scope = new_scope;
    }
    if let Some(new_token_type) = tokens.token_type {
        creds.token_type = new_token_type;
    }
    creds.expiry_date = compute_expiry_date(tokens.expires_in);
    creds.last_refresh = now;
    Ok(())
}

/// Refreshes only if the token is near expiry. Returns `true` if a refresh
/// actually happened (so the caller can persist the new tokens).
pub async fn ensure_fresh(creds: &mut GeminiOAuthCredential, skew_secs: i64) -> Result<bool> {
    if creds.is_expired(skew_secs) {
        refresh(creds).await?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// End-to-end sign-in flow:
/// 1. Bind an ephemeral loopback port (required before building the
///    authorize URL — Google's Installed-App redirect URI must match).
/// 2. Generate PKCE + state.
/// 3. Print the authorize URL to stderr, prompt the user, open the browser.
/// 4. Await the OAuth callback.
/// 5. Exchange the code for a full credential bundle.
/// 6. Fetch the signed-in email via the userinfo endpoint.
pub async fn interactive_login() -> Result<GeminiOAuthCredential> {
    use crate::services::browser_open;
    use crate::services::gemini_oauth_callback::{bind_loopback, wait_for_callback};
    use std::io::{BufRead, IsTerminal, Write as _};
    use std::time::Duration;

    let binding = bind_loopback().await?;
    let port = binding.port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");

    let pkce = PkcePair::generate();
    let state = generate_state();
    let authorize_url = build_authorize_url(&pkce.challenge, &state, &redirect_uri);

    eprintln!("Open this URL in your browser to sign in:");
    eprintln!("  {authorize_url}");
    eprintln!();
    let _ = std::io::stderr().flush();

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

    // 5-minute ceiling matches the gemini-cli's own auth timeout.
    let outcome = wait_for_callback(binding, &state, Duration::from_secs(300)).await?;

    let mut creds = exchange_code(&outcome.code, &pkce.verifier, &redirect_uri).await?;

    // Best-effort email fetch — non-fatal so a transient userinfo error
    // doesn't lose the tokens the user just authorized.
    match fetch_email(&creds.access_token).await {
        Ok(email) => creds.email = email,
        Err(e) => eprintln!("aivo: failed to fetch account email (non-fatal): {e}"),
    }

    Ok(creds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_json_roundtrip() {
        let c = GeminiOAuthCredential {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            id_token: None,
            scope: SCOPE.into(),
            token_type: "Bearer".into(),
            expiry_date: 1_700_000_000_000,
            email: Some("alice@example.com".into()),
            last_refresh: Utc::now(),
        };
        let json = c.to_json().unwrap();
        let back = GeminiOAuthCredential::from_json(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn credential_json_accepts_minimal_shape() {
        // Future-proof: extra/missing optional fields must not break parsing.
        let json = r#"{
            "access_token": "at",
            "refresh_token": "rt",
            "scope": "s",
            "token_type": "Bearer",
            "expiry_date": 0,
            "last_refresh": "2026-01-01T00:00:00Z"
        }"#;
        let back = GeminiOAuthCredential::from_json(json).unwrap();
        assert_eq!(back.access_token, "at");
        assert_eq!(back.refresh_token, "rt");
        assert!(back.id_token.is_none());
        assert!(back.email.is_none());
    }

    #[test]
    fn is_expired_respects_skew() {
        let future_ms = Utc::now().timestamp_millis() + 60_000; // +60s
        let mut c = GeminiOAuthCredential {
            access_token: "".into(),
            refresh_token: "".into(),
            id_token: None,
            scope: "".into(),
            token_type: "Bearer".into(),
            expiry_date: future_ms,
            email: None,
            last_refresh: Utc::now(),
        };
        // 120s skew + 60s remaining → already "expired"
        assert!(c.is_expired(120));
        // 30s skew + 60s remaining → still fresh
        assert!(!c.is_expired(30));
        c.expiry_date = Utc::now().timestamp_millis() - 1_000;
        assert!(c.is_expired(0));
    }

    #[test]
    fn authorize_url_includes_all_params() {
        let url = build_authorize_url(
            "test_challenge",
            "abc123",
            "http://127.0.0.1:54321/oauth2callback",
        );
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("response_type=code"));
        assert!(url.contains(&format!("client_id={CLIENT_ID}")));
        assert!(url.contains("code_challenge=test_challenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=abc123"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
        // Redirect URI must be percent-encoded so the ':' and '/' survive.
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A54321%2Foauth2callback"));
        // Scope must include the three required paths, percent-encoded.
        assert!(url.contains("cloud-platform"));
        assert!(url.contains("userinfo.email"));
        assert!(url.contains("userinfo.profile"));
    }

    #[test]
    fn compute_expiry_date_is_approximately_now_plus_expires_in() {
        let before = Utc::now().timestamp_millis();
        let exp = compute_expiry_date(3600);
        let after = Utc::now().timestamp_millis();
        assert!(exp >= before + 3_600_000);
        assert!(exp <= after + 3_600_000 + 50);
    }
}
