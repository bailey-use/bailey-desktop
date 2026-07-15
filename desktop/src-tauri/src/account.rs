use std::sync::OnceLock;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

const CLOUD_API_BASE: &str = "https://agent.meidaquan.com/api";
const CLOUD_HOST: &str = "agent.meidaquan.com";
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
// Windows Credential Manager limits generic credential blobs to 2560 bytes.
// Base64 expands by 4/3, so keep the serialized bundle below that boundary.
const MAX_SECURE_BUNDLE_BYTES: usize = 1900;
const SESSION_ENTRY: &str = "cloud-session";
const DEVICE_ID_ENTRY: &str = "device-id";

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BaileyAccount {
    pub id: String,
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuthBundle {
    pub(crate) account: BaileyAccount,
    pub(crate) session_token: String,
    pub(crate) provider: ProviderCredential,
    pub(crate) records: RecordsCredential,
    pub(crate) expires_at: String,
}

impl Drop for AuthBundle {
    fn drop(&mut self) {
        self.session_token.zeroize();
        self.provider.api_key.zeroize();
        self.records.api_key.zeroize();
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderCredential {
    pub(crate) name: String,
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) api_key: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecordsCredential {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountStatus {
    authenticated: bool,
    registration_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<BaileyAccount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

impl AccountStatus {
    fn signed_out(reason: Option<&'static str>) -> Self {
        Self {
            authenticated: false,
            registration_enabled: registration_enabled(),
            account: None,
            expires_at: None,
            reason,
        }
    }

    fn signed_in(bundle: &AuthBundle) -> Self {
        Self {
            authenticated: true,
            registration_enabled: registration_enabled(),
            account: Some(bundle.account.clone()),
            expires_at: Some(bundle.expires_at.clone()),
            reason: None,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthRequest<'a> {
    email: &'a str,
    password: &'a str,
    device_id: &'a str,
    device_name: &'a str,
    platform: &'a str,
}

#[derive(Deserialize)]
struct RemoteError {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    detail: Option<String>,
}

enum AuthAction {
    Login,
    Register,
}

enum RemoteSessionState {
    Valid,
    Revoked,
}

impl AuthAction {
    fn path(&self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Register => "register",
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Login => "登录",
            Self::Register => "注册",
        }
    }
}

#[tauri::command]
pub async fn bailey_account_status() -> Result<AccountStatus, String> {
    let Some(bundle) = load_bundle().await? else {
        return Ok(AccountStatus::signed_out(None));
    };
    if is_expired(&bundle.expires_at)? {
        clear_bundle().await?;
        return Ok(AccountStatus::signed_out(Some("expired")));
    }
    match validate_remote_session(&bundle).await? {
        RemoteSessionState::Valid => Ok(AccountStatus::signed_in(&bundle)),
        RemoteSessionState::Revoked => Ok(AccountStatus::signed_out(Some("revoked"))),
    }
}

#[tauri::command]
pub async fn bailey_account_clear_expired() -> Result<AccountStatus, String> {
    let Some(bundle) = load_bundle().await? else {
        return Ok(AccountStatus::signed_out(Some("expired")));
    };
    if !is_expired(&bundle.expires_at)? {
        return Err("Bailey 登录尚未过期，未清除本机凭据。".to_string());
    }
    // Keep expiry cleanup local so an offline Desktop can still remove all
    // scoped Cloud credentials from the OS secure store.
    clear_bundle().await?;
    Ok(AccountStatus::signed_out(Some("expired")))
}

#[tauri::command]
pub async fn bailey_account_login(
    email: String,
    password: String,
) -> Result<AccountStatus, String> {
    authenticate(AuthAction::Login, email, password).await
}

#[tauri::command]
pub async fn bailey_account_register(
    email: String,
    password: String,
) -> Result<AccountStatus, String> {
    if !registration_enabled() {
        return Err("当前 Bailey Cloud 未开放 Desktop 注册，请使用已有账号登录。".to_string());
    }
    authenticate(AuthAction::Register, email, password).await
}

#[tauri::command]
pub async fn bailey_account_logout() -> Result<AccountStatus, String> {
    if let Some(bundle) = load_bundle().await? {
        if !is_expired(&bundle.expires_at)? {
            revoke_session(&bundle).await?;
        }
    }
    clear_bundle().await?;
    Ok(AccountStatus::signed_out(None))
}

pub(crate) async fn runtime_bundle() -> Result<AuthBundle, String> {
    let bundle = load_bundle()
        .await?
        .ok_or_else(|| "请先登录 Bailey。".to_string())?;
    if is_expired(&bundle.expires_at)? {
        clear_bundle().await?;
        return Err("Bailey 登录已过期，请重新登录。".to_string());
    }
    Ok(bundle)
}

async fn authenticate(
    action: AuthAction,
    email: String,
    password: String,
) -> Result<AccountStatus, String> {
    let email = email.trim().to_string();
    let password = Zeroizing::new(password);
    validate_input(&email, password.as_str())?;
    let device_id = load_or_create_device_id().await?;
    let device_name = device_name();
    let client = cloud_client()?;
    let response = client
        .post(format!("{CLOUD_API_BASE}/auth/{}", action.path()))
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&AuthRequest {
            email: &email,
            password: password.as_str(),
            device_id: &device_id,
            device_name: &device_name,
            platform: platform_label(),
        })
        .send()
        .await
        .map_err(|_| format!("无法连接 Bailey Cloud，{}失败。", action.label()))?;
    let status = response.status();
    let body = read_limited(response).await?;
    if !status.is_success() {
        return Err(remote_error_message(action.label(), status, &body));
    }

    let bundle: AuthBundle = serde_json::from_slice(&body)
        .map_err(|_| "Bailey Cloud 返回了无法识别的账号凭据。".to_string())?;
    validate_bundle(&bundle)?;
    if is_expired(&bundle.expires_at)? {
        return Err("Bailey Cloud 返回的登录已过期，请重试。".to_string());
    }
    if let Err(error) = store_bundle(&bundle).await {
        // Do not leave an active device session behind when local secure
        // custody failed. Revocation is best effort because the original
        // storage error is the actionable result for the user.
        let _ = revoke_session(&bundle).await;
        return Err(error);
    }
    Ok(AccountStatus::signed_in(&bundle))
}

pub(crate) fn cloud_client() -> Result<reqwest::Client, String> {
    static TLS_PROVIDER_READY: OnceLock<bool> = OnceLock::new();
    let ready = TLS_PROVIDER_READY.get_or_init(|| {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        rustls::crypto::CryptoProvider::get_default().is_some()
    });
    if !*ready {
        return Err("无法初始化 Bailey Cloud TLS 加密组件。".to_string());
    }
    reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .user_agent(format!("bailey-desktop/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| "无法初始化 Bailey Cloud 安全连接。".to_string())
}

async fn read_limited(mut response: reqwest::Response) -> Result<Zeroizing<Vec<u8>>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err("Bailey Cloud 返回的数据过大。".to_string());
    }
    let mut body = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "读取 Bailey Cloud 响应失败。".to_string())?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err("Bailey Cloud 返回的数据过大。".to_string());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn remote_error_message(label: &str, status: reqwest::StatusCode, body: &[u8]) -> String {
    let remote = serde_json::from_slice::<RemoteError>(body).ok();
    let detail = remote
        .and_then(|error| error.message.or(error.error).or(error.detail))
        .map(|message| message.trim().to_string())
        .filter(|message| {
            !message.is_empty()
                && message.len() <= 300
                && !message.chars().any(char::is_control)
        });
    detail.unwrap_or_else(|| format!("{label}失败（HTTP {}）。", status.as_u16()))
}

async fn revoke_session(bundle: &AuthBundle) -> Result<(), String> {
    let client = cloud_client()?;
    let response = client
        .post(format!("{CLOUD_API_BASE}/auth/logout"))
        .bearer_auth(&bundle.session_token)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::CONTENT_LENGTH, "0")
        .send()
        .await
        .map_err(|_| {
            "无法连接 Bailey Cloud，尚未确认服务端撤销。本机登录和 Aivo Provider 访问凭据仍保留，请联网后重试退出。".to_string()
        })?;
    let status = response.status();
    if status.is_success()
        || status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::FORBIDDEN
    {
        return Ok(());
    }
    Err(format!(
        "Bailey Cloud 暂时无法撤销登录（HTTP {}）；本机登录和 Aivo Provider 访问凭据尚未删除，请联网后重试退出。",
        status.as_u16()
    ))
}

async fn validate_remote_session(bundle: &AuthBundle) -> Result<RemoteSessionState, String> {
    let client = cloud_client()?;
    let response = client
        .get(format!("{CLOUD_API_BASE}/auth/session"))
        .bearer_auth(&bundle.session_token)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| {
            "无法连接 Bailey Cloud 验证登录；本机登录仍保留，但 Agent Runtime 不会启动。请联网后重试。"
                .to_string()
        })?;
    let status = response.status();
    if status == reqwest::StatusCode::OK {
        return Ok(RemoteSessionState::Valid);
    }
    if status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::FORBIDDEN
    {
        clear_bundle().await?;
        return Ok(RemoteSessionState::Revoked);
    }

    let detail = match read_limited(response).await {
        Ok(body) => remote_error_message("验证登录", status, &body),
        Err(error) => error,
    };
    Err(format!(
        "{detail} 本机登录仍保留，但 Agent Runtime 不会启动；请联网后重试。"
    ))
}

fn validate_input(email: &str, password: &str) -> Result<(), String> {
    if email.is_empty() || email.len() > 254 || !email.contains('@') {
        return Err("请输入有效的邮箱地址。".to_string());
    }
    if password.is_empty() || password.len() > 256 {
        return Err("请输入有效的密码。".to_string());
    }
    Ok(())
}

fn registration_enabled() -> bool {
    std::env::var("BAILEY_ENABLE_ACCOUNT_REGISTRATION").as_deref() == Ok("1")
        || option_env!("BAILEY_ENABLE_ACCOUNT_REGISTRATION") == Some("1")
}

async fn load_or_create_device_id() -> Result<Zeroizing<String>, String> {
    if let Some(existing) = load_secure_value(DEVICE_ID_ENTRY).await? {
        let existing = Zeroizing::new(existing);
        if valid_device_id(&existing) {
            return Ok(existing);
        }
        return Err("系统安全存储中的 Bailey 设备标识已损坏。".to_string());
    }

    let generated = Zeroizing::new(format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    ));
    store_secure_value(DEVICE_ID_ENTRY, generated.as_str()).await?;

    // Read-back is authoritative if two first-run requests raced to create it.
    let stored = load_secure_value(DEVICE_ID_ENTRY)
        .await?
        .ok_or_else(|| "无法确认 Bailey 设备标识已写入系统安全存储。".to_string())?;
    let stored = Zeroizing::new(stored);
    if !valid_device_id(&stored) {
        return Err("系统安全存储中的 Bailey 设备标识已损坏。".to_string());
    }
    Ok(stored)
}

fn valid_device_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn device_name() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| {
            !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control)
        })
        .unwrap_or_else(|| format!("Bailey Desktop ({})", platform_label()))
}

fn platform_label() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        _ => "Desktop",
    }
}

fn validate_bundle(bundle: &AuthBundle) -> Result<(), String> {
    validate_public_value("账号 ID", &bundle.account.id, 512)?;
    validate_public_value("账号邮箱", &bundle.account.email, 320)?;
    validate_public_value("模型连接名称", &bundle.provider.name, 128)?;
    validate_public_value("模型", &bundle.provider.model, 256)?;
    validate_secret("会话凭据", &bundle.session_token)?;
    validate_secret("模型凭据", &bundle.provider.api_key)?;
    validate_secret("记录凭据", &bundle.records.api_key)?;
    if !bundle.session_token.starts_with("bss_")
        || !bundle.provider.api_key.starts_with("bmk_")
        || !bundle.records.api_key.starts_with("brk_")
        || bundle.session_token == bundle.provider.api_key
        || bundle.session_token == bundle.records.api_key
        || bundle.provider.api_key == bundle.records.api_key
    {
        return Err("Bailey Cloud 返回了无效的凭据权限边界。".to_string());
    }
    validate_cloud_url(&bundle.provider.base_url, "/v1")?;
    validate_cloud_url(&bundle.records.base_url, "/api")?;
    let _ = parse_expiry(&bundle.expires_at)?;
    Ok(())
}

fn validate_public_value(label: &str, value: &str, max: usize) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(format!("Bailey Cloud 返回的{label}无效。"));
    }
    Ok(())
}

fn validate_secret(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_SECURE_BUNDLE_BYTES {
        return Err(format!("Bailey Cloud 返回的{label}无效。"));
    }
    Ok(())
}

fn validate_cloud_url(value: &str, path: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(value)
        .map_err(|_| "Bailey Cloud 返回的服务地址无效。".to_string())?;
    let valid = url.scheme() == "https"
        && url.host_str() == Some(CLOUD_HOST)
        && url.port().is_none_or(|port| port == 443)
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.path().trim_end_matches('/') == path;
    if !valid {
        return Err("Bailey Cloud 返回了不受信任的服务地址。".to_string());
    }
    Ok(())
}

fn parse_expiry(value: &str) -> Result<chrono::DateTime<chrono::FixedOffset>, String> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map_err(|_| "Bailey Cloud 返回的登录有效期无效。".to_string())
}

fn is_expired(value: &str) -> Result<bool, String> {
    Ok(parse_expiry(value)?.with_timezone(&Utc) <= Utc::now())
}

async fn store_bundle(bundle: &AuthBundle) -> Result<(), String> {
    let json = Zeroizing::new(
        serde_json::to_vec(bundle)
            .map_err(|_| "无法准备 Bailey 账号安全凭据。".to_string())?,
    );
    if json.len() > MAX_SECURE_BUNDLE_BYTES {
        return Err("Bailey 账号安全凭据过大。".to_string());
    }
    let encoded = Zeroizing::new(BASE64.encode(json.as_slice()));
    store_secure_value(SESSION_ENTRY, encoded.as_str()).await
}

async fn load_bundle() -> Result<Option<AuthBundle>, String> {
    let encoded = load_secure_value(SESSION_ENTRY).await?;
    let Some(encoded) = encoded else {
        return Ok(None);
    };
    let encoded = Zeroizing::new(encoded);
    let json = Zeroizing::new(
        BASE64
            .decode(encoded.trim())
            .map_err(|_| "系统安全存储中的 Bailey 账号凭据已损坏。".to_string())?,
    );
    if json.len() > MAX_SECURE_BUNDLE_BYTES {
        return Err("系统安全存储中的 Bailey 账号凭据过大。".to_string());
    }
    let bundle = serde_json::from_slice::<AuthBundle>(&json)
        .map_err(|_| "系统安全存储中的 Bailey 账号凭据已损坏。".to_string())?;
    validate_bundle(&bundle)?;
    Ok(Some(bundle))
}

async fn clear_bundle() -> Result<(), String> {
    let entry = SESSION_ENTRY.to_string();
    tauri::async_runtime::spawn_blocking(move || secure_store::clear(&entry))
        .await
        .map_err(|_| "系统安全存储不可用。".to_string())?
}

async fn load_secure_value(entry: &str) -> Result<Option<String>, String> {
    let entry = entry.to_string();
    tauri::async_runtime::spawn_blocking(move || secure_store::load(&entry))
        .await
        .map_err(|_| "系统安全存储不可用。".to_string())?
}

async fn store_secure_value(entry: &str, value: &str) -> Result<(), String> {
    let entry = entry.to_string();
    let value = Zeroizing::new(value.to_string());
    tauri::async_runtime::spawn_blocking(move || secure_store::store(&entry, value.as_str()))
        .await
        .map_err(|_| "系统安全存储不可用。".to_string())?
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod secure_subprocess {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use zeroize::Zeroizing;

    const WATCHDOG: Duration = Duration::from_secs(10);

    pub(super) struct RunOutput {
        pub success: bool,
        pub code: Option<i32>,
        pub stdout: Zeroizing<String>,
        pub stderr: String,
    }

    pub(super) fn run(mut command: Command, stdin: Option<&str>) -> Option<RunOutput> {
        command
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().ok()?;
        if let Some(input) = stdin
            && let Some(mut child_stdin) = child.stdin.take()
        {
            let _ = child_stdin.write_all(input.as_bytes());
        }
        let deadline = Instant::now() + WATCHDOG;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let mut stdout = String::new();
                    let mut stderr = String::new();
                    if let Some(mut pipe) = child.stdout.take() {
                        let _ = pipe.read_to_string(&mut stdout);
                    }
                    if let Some(mut pipe) = child.stderr.take() {
                        let _ = pipe.read_to_string(&mut stderr);
                    }
                    return Some(RunOutput {
                        success: status.success(),
                        code: status.code(),
                        stdout: Zeroizing::new(stdout),
                        stderr,
                    });
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                Err(_) => return None,
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod secure_store {
    use std::process::Command;

    use super::secure_subprocess::run;
    use zeroize::Zeroizing;

    const SERVICE: &str = "Bailey";

    pub(super) fn load(account: &str) -> Result<Option<String>, String> {
        let mut command = Command::new("/usr/bin/security");
        command.args(["find-generic-password", "-s", SERVICE, "-a", account, "-w"]);
        let output = run(command, None).ok_or_else(unavailable)?;
        if output.success {
            return Ok(Some(output.stdout.trim().to_string()));
        }
        if output.code == Some(44) || output.stderr.contains("specified item could not be found") {
            Ok(None)
        } else {
            Err(unavailable())
        }
    }

    pub(super) fn store(account: &str, value: &str) -> Result<(), String> {
        let mut command = Command::new("/usr/bin/security");
        command.arg("-i");
        let script = Zeroizing::new(format!(
            "add-generic-password -U -s \"{SERVICE}\" -a \"{account}\" -w \"{value}\" -T \"/usr/bin/security\"\n"
        ));
        match run(command, Some(script.as_str())) {
            Some(output) if output.success => Ok(()),
            _ => Err(unavailable()),
        }
    }

    pub(super) fn clear(account: &str) -> Result<(), String> {
        let mut command = Command::new("/usr/bin/security");
        command.args(["delete-generic-password", "-s", SERVICE, "-a", account]);
        let output = run(command, None).ok_or_else(unavailable)?;
        if output.success
            || output.code == Some(44)
            || output.stderr.contains("specified item could not be found")
        {
            Ok(())
        } else {
            Err(unavailable())
        }
    }

    fn unavailable() -> String {
        "macOS Keychain 不可用，Bailey 不会把凭据写入明文文件。".to_string()
    }
}

#[cfg(target_os = "linux")]
mod secure_store {
    use std::process::Command;

    use super::secure_subprocess::run;

    const SERVICE: &str = "bailey";

    pub(super) fn load(account: &str) -> Result<Option<String>, String> {
        let mut command = Command::new("secret-tool");
        command.args(["lookup", "service", SERVICE, "account", account]);
        let output = run(command, None).ok_or_else(unavailable)?;
        if output.success {
            return Ok(Some(output.stdout.trim().to_string()));
        }
        if output.code == Some(1) && output.stderr.trim().is_empty() {
            Ok(None)
        } else {
            Err(unavailable())
        }
    }

    pub(super) fn store(account: &str, value: &str) -> Result<(), String> {
        let mut command = Command::new("secret-tool");
        command.args([
            "store",
            "--label",
            "Bailey Cloud session",
            "service",
            SERVICE,
            "account",
            account,
        ]);
        match run(command, Some(value)) {
            Some(output) if output.success => Ok(()),
            _ => Err(unavailable()),
        }
    }

    pub(super) fn clear(account: &str) -> Result<(), String> {
        let mut command = Command::new("secret-tool");
        command.args(["clear", "service", SERVICE, "account", account]);
        match run(command, None) {
            Some(output) if output.success || output.code == Some(1) => Ok(()),
            _ => Err(unavailable()),
        }
    }

    fn unavailable() -> String {
        "Linux Secret Service 不可用，Bailey 不会把凭据写入明文文件。".to_string()
    }
}

#[cfg(target_os = "windows")]
mod secure_store {
    use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, FILETIME, GetLastError};
    use windows_sys::Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
        CredReadW, CredWriteW,
    };
    use zeroize::Zeroize;

    const MAX_CREDENTIAL_BLOB: usize = 2560;

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(super) fn load(account: &str) -> Result<Option<String>, String> {
        let target = wide(&format!("Bailey/{account}"));
        let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
        let ok = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
        if ok == 0 {
            return if unsafe { GetLastError() } == ERROR_NOT_FOUND {
                Ok(None)
            } else {
                Err(unavailable())
            };
        }
        let value = unsafe {
            let pointer = (*credential).CredentialBlob;
            let length = (*credential).CredentialBlobSize as usize;
            if pointer.is_null() || length == 0 || length > MAX_CREDENTIAL_BLOB {
                None
            } else {
                String::from_utf8(std::slice::from_raw_parts(pointer, length).to_vec()).ok()
            }
        };
        unsafe { CredFree(credential.cast()) };
        value.map(Some).ok_or_else(unavailable)
    }

    pub(super) fn store(account: &str, value: &str) -> Result<(), String> {
        if value.len() > MAX_CREDENTIAL_BLOB {
            return Err("Bailey 账号凭据超过 Windows Credential Manager 限制。".to_string());
        }
        let target = wide(&format!("Bailey/{account}"));
        let username = wide("Bailey");
        let mut blob = value.as_bytes().to_vec();
        let credential = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_ptr() as *mut u16,
            Comment: std::ptr::null_mut(),
            LastWritten: FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            },
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: std::ptr::null_mut(),
            TargetAlias: std::ptr::null_mut(),
            UserName: username.as_ptr() as *mut u16,
        };
        let stored = unsafe { CredWriteW(&credential, 0) != 0 };
        blob.zeroize();
        if stored { Ok(()) } else { Err(unavailable()) }
    }

    pub(super) fn clear(account: &str) -> Result<(), String> {
        let target = wide(&format!("Bailey/{account}"));
        let deleted = unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
        if deleted != 0 || unsafe { GetLastError() } == ERROR_NOT_FOUND {
            Ok(())
        } else {
            Err(unavailable())
        }
    }

    fn unavailable() -> String {
        "Windows Credential Manager 不可用，Bailey 不会把凭据写入明文文件。".to_string()
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod secure_store {
    fn unavailable() -> String {
        "当前系统没有 Bailey 支持的安全凭据存储；不会使用明文回退。".to_string()
    }

    pub(super) fn load(_account: &str) -> Result<Option<String>, String> {
        Err(unavailable())
    }

    pub(super) fn store(_account: &str, _value: &str) -> Result<(), String> {
        Err(unavailable())
    }

    pub(super) fn clear(_account: &str) -> Result<(), String> {
        Err(unavailable())
    }
}
