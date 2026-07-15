use serde_json::Value;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_shell::ShellExt;
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tokio::sync::{Mutex, watch};

#[derive(Default)]
pub struct AppServerState {
    inner: Mutex<AppServerStateInner>,
}

#[derive(Default)]
struct AppServerStateInner {
    next_generation: u64,
    child: Option<RunningChild>,
}

struct RunningChild {
    generation: u64,
    process: TrackedChild,
    cloud_records: crate::cloud_records::CloudRecordRelay,
    stopping: bool,
    termination_observed: watch::Receiver<bool>,
    shutdown_completed: watch::Receiver<bool>,
}

/// Keep ownership of the plugin child until its `Terminated` event is seen.
///
/// `tauri-plugin-shell` 2.3.5 exposes `CommandChild::kill(self)`: even an
/// unsuccessful kill consumes the only public control handle. Normal shutdown
/// therefore uses the non-consuming PID path below and retains this wrapper so
/// a later stop request can retry. Dropping is only the final safety net; the
/// plugin kill is PID-reuse-safe because it operates through `SharedChild`.
struct TrackedChild {
    process: Option<CommandChild>,
}

impl TrackedChild {
    fn new(process: CommandChild) -> Self {
        Self {
            process: Some(process),
        }
    }

    fn pid(&self) -> u32 {
        self.process
            .as_ref()
            .expect("tracked child must retain its process handle")
            .pid()
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.process
            .as_mut()
            .expect("tracked child must retain its process handle")
            .write(bytes)
            .map_err(|error| error.to_string())
    }

    fn final_kill(&mut self) -> Result<(), String> {
        self.process
            .take()
            .expect("tracked child must retain its process handle")
            .kill()
            .map_err(|error| error.to_string())
    }
}

impl Drop for TrackedChild {
    fn drop(&mut self) {
        if let Some(process) = self.process.take() {
            let _ = process.kill();
        }
    }
}

#[tauri::command]
pub async fn app_server_start(
    app: AppHandle,
    state: State<'_, AppServerState>,
) -> Result<(), String> {
    // Keep start and stop serialized across secure-store access and provider
    // provisioning. A stop request must never observe an empty slot and return
    // while an older start is still able to spawn an untracked sidecar.
    let mut state = state.inner.lock().await;
    if let Some(child) = state.child.as_ref() {
        return if child.stopping {
            Err("Agent runtime is still completing shutdown".to_string())
        } else {
            Ok(())
        };
    }

    // Bailey account credentials never cross the WebView/App Server protocol.
    // A short-lived Aivo process receives the model key only long enough to
    // update Aivo's existing encrypted Provider store. The actual Agent Runtime
    // starts afterwards without either Cloud key in its environment, so agent
    // shells, jobs, hooks, and MCP children cannot inherit those credentials.
    let credentials = crate::account::runtime_bundle().await?;
    provision_aivo_provider(&app, &credentials).await?;
    let cloud_records = crate::cloud_records::CloudRecordRelay::start(
        app.clone(),
        &credentials.records,
    )?;

    let mut command = app
        .shell()
        .sidecar("aivo-app-server")
        .map_err(|error| error.to_string())?
        .args(["app-server", "--stdio"])
        .env(
            "BAILEY_CLOUD_MODEL_BASE_URL",
            credentials.provider.base_url.as_str(),
        )
        .env("BAILEY_CLOUD_MODEL", credentials.provider.model.as_str())
        // Explicitly shadow any developer shell values inherited by Desktop.
        // Cloud record sync must be owned by Desktop, not an Aivo environment.
        .env("BAILEY_CLOUD_MODEL_API_KEY", "")
        .env("BAILEY_CLOUD_RECORDS_API_KEY", "")
        .env("BAILEY_DISABLE_CLOUD_RECORDS", "1")
        .env("BAILEY_ENABLE_DEFAULT_PROVIDER", "1");
    let runtime = prepare_integrated_runtime(&app);
    if std::env::var_os("BAILEY_LOCAL_MCP_COMMAND").is_none()
        && let Some((node, server)) = runtime.local_tools.as_ref()
    {
        command = command
            .env("BAILEY_LOCAL_MCP_COMMAND", node)
            .env(
                "BAILEY_LOCAL_MCP_ARGS_JSON",
                serde_json::to_string(&[server.to_string_lossy().as_ref()])
                    .map_err(|error| error.to_string())?,
            );
    } else if std::env::var_os("BAILEY_LOCAL_MCP_COMMAND").is_none()
        && let Some(path) = discover_local_tools_command(&app)
    {
        command = command.env("BAILEY_LOCAL_MCP_COMMAND", path);
    }
    let cua_driver = runtime
        .cua_driver
        .clone()
        .or_else(|| discover_cua_driver(&app));
    if std::env::var_os("BAILEY_CUA_DRIVER_SOCKET").is_none()
        && std::env::var_os("BAILEY_CUA_DRIVER_COMMAND").is_none()
        && let Some(path) = cua_driver.as_ref()
    {
        command = command.env("BAILEY_CUA_DRIVER_COMMAND", quote_command_path(&path));
    }
    command = command.env(
        "BAILEY_RUNTIME_DIAGNOSTICS_JSON",
        runtime.diagnostics.to_string(),
    )
    .env("BAILEY_DESKTOP_VERSION", env!("CARGO_PKG_VERSION"));
    let (mut events, process) = command.spawn().map_err(|error| error.to_string())?;
    let (termination_tx, termination_rx) = watch::channel(false);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let generation = state.next_generation;
    state.next_generation = state.next_generation.wrapping_add(1);
    state.child = Some(RunningChild {
        generation,
        process: TrackedChild::new(process),
        cloud_records,
        stopping: false,
        termination_observed: termination_rx,
        shutdown_completed: shutdown_rx,
    });
    drop(state);

    let event_app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut stdout = String::new();
        while let Some(event) = events.recv().await {
            let app_state = event_app.state::<AppServerState>();
            let mut state = app_state.inner.lock().await;
            if state
                .child
                .as_ref()
                .is_none_or(|child| child.generation != generation)
            {
                break;
            }
            let cloud_records = state
                .child
                .as_ref()
                .map(|child| child.cloud_records.clone())
                .expect("generation-matched runtime must have a record relay");
            match event {
                CommandEvent::Stdout(bytes) => {
                    stdout.push_str(&String::from_utf8_lossy(&bytes));
                    emit_complete_lines(&event_app, &cloud_records, &mut stdout);
                }
                CommandEvent::Stderr(bytes) => {
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    let _ = event_app.emit("app-server://stderr", text);
                }
                CommandEvent::Error(error) => {
                    let _ = event_app.emit("app-server://stderr", error);
                }
                CommandEvent::Terminated(payload) => {
                    // Signal the exact process exit before draining Cloud
                    // records. Stop must never send a PID-based kill merely
                    // because the already-dead process's relay is still
                    // completing its bounded shutdown.
                    let _ = termination_tx.send(true);
                    if let Some(child) = state.child.as_mut() {
                        child.stopping = true;
                    }
                    if !stdout.trim().is_empty() {
                        let line = stdout.trim().to_string();
                        cloud_records.observe_line(&line);
                        let _ = event_app.emit("app-server://message", line);
                    }
                    drop(state);
                    cloud_records.shutdown().await;
                    let app_state = event_app.state::<AppServerState>();
                    let mut state = app_state.inner.lock().await;
                    if state
                        .child
                        .as_ref()
                        .is_some_and(|child| child.generation == generation)
                    {
                        state.child = None;
                    }
                    drop(state);
                    let _ = event_app.emit("app-server://exit", payload);
                    let _ = shutdown_tx.send(true);
                    break;
                }
                _ => {}
            }
        }
    });
    Ok(())
}

const PROVIDER_PROVISION_TIMEOUT: Duration = Duration::from_secs(15);
const PROVIDER_PROVISION_OUTPUT_LIMIT: usize = 64 * 1024;
const PROVIDER_TERMINATION_RETRY_INTERVAL: Duration = Duration::from_secs(2);
const PROVIDER_TERMINATION_FOREGROUND_TIMEOUT: Duration = Duration::from_secs(6);
const PROVIDER_TERMINATION_BACKGROUND_TIMEOUT: Duration = Duration::from_secs(8);
const PROVIDER_TERMINATION_FINAL_CONFIRM_TIMEOUT: Duration = Duration::from_secs(3);

enum ProviderProvisionOutcome {
    Completed,
    TerminatedWithError(String),
    StillRunningWithError(String),
}

/// Provision Aivo through its existing App Server startup boundary, then exit.
///
/// This deliberately avoids adding a Bailey account API to Aivo. The model key
/// is visible only to this task-less process; the long-running Agent Runtime is
/// spawned separately with the sensitive environment variables blanked.
async fn provision_aivo_provider(
    app: &AppHandle,
    credentials: &crate::account::AuthBundle,
) -> Result<(), String> {
    let command = app
        .shell()
        .sidecar("aivo-app-server")
        .map_err(|error| error.to_string())?
        .args(["app-server", "--stdio"])
        .env(
            "BAILEY_CLOUD_MODEL_API_KEY",
            credentials.provider.api_key.as_str(),
        )
        .env(
            "BAILEY_CLOUD_MODEL_BASE_URL",
            credentials.provider.base_url.as_str(),
        )
        .env("BAILEY_CLOUD_MODEL", credentials.provider.model.as_str())
        .env("BAILEY_CLOUD_RECORDS_API_KEY", "")
        .env("BAILEY_DISABLE_CLOUD_RECORDS", "1")
        .env("BAILEY_ENABLE_DEFAULT_PROVIDER", "1");
    let (mut events, process) = command.spawn().map_err(|error| error.to_string())?;
    let mut process = TrackedChild::new(process);
    let shutdown = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": "bailey:provider-provision",
        "method": "shutdown",
        "params": {},
    }))
    .map_err(|error| error.to_string())?;
    let mut frame = shutdown;
    frame.push(b'\n');
    if let Err(error) = process.write(&frame) {
        let cleanup = terminate_provider_process(process, events).await;
        return Err(provider_error_with_cleanup(
            format!("无法把 Bailey 模型连接写入 Aivo Provider：{error}"),
            cleanup,
        ));
    }

    let result = tokio::time::timeout(PROVIDER_PROVISION_TIMEOUT, async {
        let mut stdout = String::new();
        let mut stderr = String::new();
        while let Some(event) = events.recv().await {
            match event {
                CommandEvent::Stdout(bytes) => {
                    append_limited_output(&mut stdout, &bytes);
                }
                CommandEvent::Stderr(bytes) => {
                    append_limited_output(&mut stderr, &bytes);
                }
                CommandEvent::Error(error) => {
                    return ProviderProvisionOutcome::StillRunningWithError(format!(
                        "Aivo Provider 配置进程失败：{error}"
                    ));
                }
                CommandEvent::Terminated(payload) => {
                    if payload.code != Some(0) {
                        return ProviderProvisionOutcome::TerminatedWithError(
                            "Aivo Provider 配置进程异常退出。".to_string(),
                        );
                    }
                    if stderr.contains("could not prepare the default model provider") {
                        return ProviderProvisionOutcome::TerminatedWithError(
                            "Aivo 拒绝保存 Bailey 模型连接。".to_string(),
                        );
                    }
                    if !stdout.contains("bailey:provider-provision") {
                        return ProviderProvisionOutcome::TerminatedWithError(
                            "Aivo 未确认 Bailey Provider 配置完成。".to_string(),
                        );
                    }
                    return ProviderProvisionOutcome::Completed;
                }
                _ => {}
            }
        }
        ProviderProvisionOutcome::StillRunningWithError(
            "Aivo Provider 配置进程意外关闭。".to_string(),
        )
    })
    .await;

    match result {
        Ok(ProviderProvisionOutcome::Completed) => Ok(()),
        Ok(ProviderProvisionOutcome::TerminatedWithError(error)) => Err(error),
        Ok(ProviderProvisionOutcome::StillRunningWithError(error)) => {
            let cleanup = terminate_provider_process(process, events).await;
            Err(provider_error_with_cleanup(error, cleanup))
        }
        Err(_) => {
            let cleanup = terminate_provider_process(process, events).await;
            Err(provider_error_with_cleanup(
                "Aivo Provider 配置超时，Agent Runtime 未启动。".to_string(),
                cleanup,
            ))
        }
    }
}

fn provider_error_with_cleanup(error: String, cleanup: Result<(), String>) -> String {
    match cleanup {
        Ok(()) => error,
        Err(cleanup_error) => format!("{error} {cleanup_error}"),
    }
}

/// Give the foreground a strict deadline to confirm termination. If the event
/// channel cannot confirm it, transfer both the child handle and receiver to a
/// bounded cleanup task; the login command must not hang forever or drop an
/// untracked model-key process.
async fn terminate_provider_process(
    mut process: TrackedChild,
    mut events: tauri::async_runtime::Receiver<CommandEvent>,
) -> Result<(), String> {
    if attempt_provider_termination(
        &mut process,
        &mut events,
        PROVIDER_TERMINATION_FOREGROUND_TIMEOUT,
    )
    .await
    {
        return Ok(());
    }

    tauri::async_runtime::spawn(async move {
        if attempt_provider_termination(
            &mut process,
            &mut events,
            PROVIDER_TERMINATION_BACKGROUND_TIMEOUT,
        )
        .await
        {
            return;
        }

        // This final call uses the plugin's SharedChild handle, so it cannot
        // target a recycled PID. It is intentionally last because the plugin
        // API consumes CommandChild even when the kill itself fails.
        let _ = process.final_kill();
        let _ = tokio::time::timeout(
            PROVIDER_TERMINATION_FINAL_CONFIRM_TIMEOUT,
            receive_provider_termination(&mut events),
        )
        .await;
    });

    Err("Aivo Provider 终止尚未确认；已交给有界后台清理，Agent Runtime 不会启动。".to_string())
}

async fn attempt_provider_termination(
    process: &mut TrackedChild,
    events: &mut tauri::async_runtime::Receiver<CommandEvent>,
    total_timeout: Duration,
) -> bool {
    tokio::time::timeout(total_timeout, async {
        loop {
            let _ = force_terminate_process(process.pid()).await;
            match tokio::time::timeout(
                PROVIDER_TERMINATION_RETRY_INTERVAL,
                receive_provider_termination(events),
            )
            .await
            {
                Ok(true) => return true,
                Ok(false) => tokio::time::sleep(PROVIDER_TERMINATION_RETRY_INTERVAL).await,
                Err(_) => {}
            }
        }
    })
    .await
    .unwrap_or(false)
}

async fn receive_provider_termination(
    events: &mut tauri::async_runtime::Receiver<CommandEvent>,
) -> bool {
    while let Some(event) = events.recv().await {
        if matches!(event, CommandEvent::Terminated(_)) {
            return true;
        }
    }
    false
}

fn append_limited_output(target: &mut String, bytes: &[u8]) {
    if target.len() >= PROVIDER_PROVISION_OUTPUT_LIMIT {
        return;
    }
    let value = String::from_utf8_lossy(bytes);
    for character in value.chars() {
        if target.len() + character.len_utf8() > PROVIDER_PROVISION_OUTPUT_LIMIT {
            break;
        }
        target.push(character);
    }
}

#[derive(Default)]
struct IntegratedRuntime {
    local_tools: Option<(PathBuf, PathBuf)>,
    cua_driver: Option<PathBuf>,
    diagnostics: Value,
}

fn prepare_integrated_runtime(app: &AppHandle) -> IntegratedRuntime {
    let mut issues = Vec::new();
    let source = app
        .path()
        .resource_dir()
        .ok()
        .map(|root| root.join("bailey-runtime"));
    let Some(source) = source.filter(|root| root.join("manifest.json").is_file()) else {
        return IntegratedRuntime {
            diagnostics: serde_json::json!({
                "bundled": false,
                "compatible": false,
                "issues": ["integrated runtime resource is missing"]
            }),
            ..Default::default()
        };
    };
    let manifest = fs::read_to_string(source.join("manifest.json")).unwrap_or_default();
    let manifest_json: Value = serde_json::from_str(&manifest).unwrap_or(Value::Null);
    let version_compatible = manifest_json
        .get("desktopVersion")
        .and_then(Value::as_str)
        .is_some_and(|version| version == env!("CARGO_PKG_VERSION"));
    if !version_compatible {
        issues.push("integrated runtime version is incompatible".to_string());
    }
    let source_integrity = runtime_files_match(&source, &manifest_json);
    if !source_integrity {
        issues.push("integrated runtime resource failed integrity validation".to_string());
    }
    let compatible = version_compatible && source_integrity;
    if !compatible {
        return IntegratedRuntime {
            diagnostics: serde_json::json!({
                "bundled": true,
                "compatible": false,
                "integrity": source_integrity,
                "issues": issues,
            }),
            ..Default::default()
        };
    }
    let destination = app
        .path()
        .local_data_dir()
        .ok()
        .map(|root| root.join("Bailey").join("runtime").join(env!("CARGO_PKG_VERSION")));
    let Some(destination) = destination else {
        issues.push("local application-data directory is unavailable".to_string());
        return IntegratedRuntime {
            diagnostics: serde_json::json!({ "bundled": true, "compatible": compatible, "issues": issues }),
            ..Default::default()
        };
    };
    let destination_current = fs::read_to_string(destination.join("manifest.json"))
        .ok()
        .as_deref()
        == Some(manifest.as_str())
        && runtime_files_match(&destination, &manifest_json);
    if !destination_current {
        let temporary = destination.with_extension(format!("tmp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temporary);
        if copy_tree(&source, &temporary).is_err() {
            issues.push("integrated runtime could not be installed".to_string());
        } else {
            let _ = fs::remove_dir_all(&destination);
            if fs::rename(&temporary, &destination).is_err() {
                issues.push("integrated runtime install could not be finalized".to_string());
            }
        }
    }
    let installed_integrity = fs::read_to_string(destination.join("manifest.json"))
        .ok()
        .as_deref()
        == Some(manifest.as_str())
        && runtime_files_match(&destination, &manifest_json);
    if !installed_integrity {
        issues.push("installed runtime failed integrity validation".to_string());
    }
    #[cfg(windows)]
    let node = destination.join("node").join("node.exe");
    #[cfg(not(windows))]
    let node = destination.join("node").join("node");
    let server = destination.join("src").join("mcp").join("server.js");
    if installed_integrity && node.is_file() && server.is_file() {
        let installer = destination.join("scripts").join("browser-install.mjs");
        if installer.is_file() {
            match Command::new(&node)
                .arg(&installer)
                .arg("--browser=all")
                .status()
            {
                Ok(status) if status.success() => {}
                _ => issues.push("browser native host registration failed".to_string()),
            }
        }
    } else {
        issues.push("Bailey Local Tools launcher is incomplete".to_string());
    }
    #[cfg(windows)]
    let cua_driver = destination.join("computer-use").join("driver").join("cua-driver.exe");
    #[cfg(not(windows))]
    let cua_driver = destination.join("computer-use").join("driver").join("cua-driver");
    if !cua_driver.is_file() {
        issues.push("computer-use driver is missing".to_string());
    }
    let local_tools_available = installed_integrity && node.is_file() && server.is_file();
    let cua_available = installed_integrity && cua_driver.is_file();
    IntegratedRuntime {
        local_tools: local_tools_available.then_some((node, server)),
        cua_driver: cua_available.then_some(cua_driver),
        diagnostics: serde_json::json!({
            "bundled": true,
            "compatible": compatible,
            "integrity": installed_integrity,
            "localTools": local_tools_available,
            "nativeHost": installed_integrity && destination.join("native-host").is_dir(),
            "extension": installed_integrity && destination.join("extension").is_dir(),
            "cuaDriver": cua_available,
            "issues": issues,
        }),
    }
}

fn copy_tree(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn runtime_files_match(root: &Path, manifest: &Value) -> bool {
    let Some(files) = manifest.get("files").and_then(Value::as_array) else {
        return false;
    };
    if files.is_empty() {
        return false;
    }
    files.iter().all(|entry| {
        let Some(relative) = entry.get("path").and_then(Value::as_str) else {
            return false;
        };
        let relative = Path::new(relative);
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return false;
        }
        let Some(expected_size) = entry.get("size").and_then(Value::as_u64) else {
            return false;
        };
        let Some(expected_hash) = entry.get("sha256").and_then(Value::as_str) else {
            return false;
        };
        let hash_is_hex = expected_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit());
        if expected_hash.len() != 64 || !hash_is_hex {
            return false;
        }
        let absolute = root.join(relative);
        let Ok(metadata) = fs::symlink_metadata(&absolute) else {
            return false;
        };
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() != expected_size
        {
            return false;
        }
        let Ok(mut file) = fs::File::open(absolute) else {
            return false;
        };
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let Ok(read) = file.read(&mut buffer) else {
                return false;
            };
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(expected_hash)
    })
}

fn discover_local_tools_command(app: &AppHandle) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(root) = std::env::var_os("BAILEY_USE_INSTALL_ROOT") {
        push_local_tools_candidates(&mut candidates, PathBuf::from(root));
    }
    if let Ok(data_root) = app.path().local_data_dir() {
        push_local_tools_candidates(&mut candidates, data_root.join("BaileyUse"));
    }
    candidates.into_iter().find(|path| is_launchable(path))
}

fn push_local_tools_candidates(candidates: &mut Vec<PathBuf>, root: PathBuf) {
    #[cfg(windows)]
    {
        candidates.push(root.join("bailey-mcp.cmd"));
        candidates.push(root.join("browser-host").join("bailey-mcp.cmd"));
    }
    #[cfg(not(windows))]
    {
        candidates.push(root.join("browser-host").join("bailey-mcp"));
        candidates.push(root.join("bailey-mcp"));
    }
}

fn discover_cua_driver(app: &AppHandle) -> Option<PathBuf> {
    let data_root = app.path().local_data_dir().ok()?;
    #[cfg(windows)]
    let driver = data_root
        .join("BaileyUseComputerUse")
        .join("bin")
        .join("cua-driver.exe");
    #[cfg(not(windows))]
    let driver = data_root
        .join("BaileyUseComputerUse")
        .join("bin")
        .join("cua-driver");
    is_launchable(&driver).then_some(driver)
}

fn is_launchable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn quote_command_path(path: &Path) -> OsString {
    let mut quoted = OsString::from("\"");
    quoted.push(path.as_os_str());
    quoted.push("\"");
    quoted
}

#[tauri::command]
pub async fn app_server_send(
    message: Value,
    state: State<'_, AppServerState>,
) -> Result<(), String> {
    let mut encoded = serde_json::to_vec(&message).map_err(|error| error.to_string())?;
    encoded.push(b'\n');
    let mut state = state.inner.lock().await;
    let process = state
        .child
        .as_mut()
        .ok_or_else(|| "Agent runtime is not running".to_string())?;
    if process.stopping {
        return Err("Agent runtime is stopping".to_string());
    }
    process
        .process
        .write(&encoded)
        .map_err(|error| error.to_string())
}

const APP_SERVER_GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(3);
const APP_SERVER_FORCE_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const APP_SERVER_RELAY_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(14);
const FORCE_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);

#[tauri::command]
pub async fn app_server_stop(state: State<'_, AppServerState>) -> Result<(), String> {
    let (pid, mut termination_observed, mut shutdown_completed, wait_gracefully) = {
        let mut state = state.inner.lock().await;
        let Some(child) = state.child.as_mut() else {
            return Ok(());
        };
        let first_stop_request = !child.stopping;
        child.stopping = true;
        let shutdown_written = if first_stop_request {
            let mut frame = serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": format!("bailey:desktop-stop:{}", child.generation),
                "method": "shutdown",
                "params": {},
            }))
            .map_err(|error| error.to_string())?;
            frame.push(b'\n');
            child.process.write(&frame).is_ok()
        } else {
            true
        };
        (
            child.process.pid(),
            child.termination_observed.clone(),
            child.shutdown_completed.clone(),
            shutdown_written,
        )
    };

    let mut process_terminated = wait_gracefully
        && wait_for_confirmation(
            &mut termination_observed,
            APP_SERVER_GRACEFUL_STOP_TIMEOUT,
        )
        .await;
    let mut termination_error = None;
    if !process_terminated {
        termination_error = force_terminate_process(pid).await.err();
        process_terminated = wait_for_confirmation(
            &mut termination_observed,
            APP_SERVER_FORCE_STOP_TIMEOUT,
        )
        .await;
    }

    if !process_terminated {
        let detail = termination_error
            .map(|error| format!(" 强制终止请求失败：{error}"))
            .unwrap_or_default();
        return Err(format!(
            "无法确认 Agent runtime 已停止；进程句柄仍被保留，可重试停止。{detail}"
        ));
    }

    if wait_for_confirmation(
        &mut shutdown_completed,
        APP_SERVER_RELAY_SHUTDOWN_TIMEOUT,
    )
    .await
    {
        Ok(())
    } else {
        Err("Agent runtime 已停止，但 Cloud 记录清理未在期限内确认完成。".to_string())
    }
}

async fn wait_for_confirmation(
    confirmation: &mut watch::Receiver<bool>,
    timeout: Duration,
) -> bool {
    if *confirmation.borrow() {
        return true;
    }
    tokio::time::timeout(timeout, async {
        loop {
            if confirmation.changed().await.is_err() {
                return false;
            }
            if *confirmation.borrow_and_update() {
                return true;
            }
        }
    })
    .await
    .unwrap_or(false)
}

async fn force_terminate_process(pid: u32) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || force_terminate_process_blocking(pid))
        .await
        .map_err(|error| format!("强制终止任务失败：{error}"))?
}

#[cfg(unix)]
fn force_terminate_process_blocking(pid: u32) -> Result<(), String> {
    let mut command = Command::new("/bin/kill");
    command.args(["-KILL", &pid.to_string()]);
    run_force_command(command, "/bin/kill")
}

#[cfg(windows)]
fn force_terminate_process_blocking(pid: u32) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    let mut command = Command::new("taskkill.exe");
    command.args(["/PID", &pid.to_string(), "/T", "/F"]);
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    run_force_command(command, "taskkill.exe")
}

fn run_force_command(mut command: Command, label: &str) -> Result<(), String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|error| format!("无法执行 {label}：{error}"))?;
    let deadline = Instant::now() + FORCE_COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("{label} 返回 {status}")),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{label} 强制终止命令超时"));
            }
            Err(error) => return Err(format!("无法确认 {label} 执行结果：{error}")),
        }
    }
}

fn emit_complete_lines(
    app: &AppHandle,
    cloud_records: &crate::cloud_records::CloudRecordRelay,
    buffer: &mut String,
) {
    while let Some(newline) = buffer.find('\n') {
        let line = buffer[..newline].trim_end_matches('\r').to_string();
        buffer.drain(..=newline);
        if !line.is_empty() {
            cloud_records.observe_line(&line);
            let _ = app.emit("app-server://message", line);
        }
    }
}
