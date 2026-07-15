use serde_json::Value;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_shell::ShellExt;
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tokio::sync::Mutex;

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
    process: CommandChild,
}

#[tauri::command]
pub async fn app_server_start(
    app: AppHandle,
    state: State<'_, AppServerState>,
) -> Result<(), String> {
    let mut state = state.inner.lock().await;
    if state.child.is_some() {
        return Ok(());
    }

    let mut command = app
        .shell()
        .sidecar("aivo-app-server")
        .map_err(|error| error.to_string())?
        .args(["app-server", "--stdio"]);
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
    let generation = state.next_generation;
    state.next_generation = state.next_generation.wrapping_add(1);
    state.child = Some(RunningChild {
        generation,
        process,
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
            match event {
                CommandEvent::Stdout(bytes) => {
                    stdout.push_str(&String::from_utf8_lossy(&bytes));
                    emit_complete_lines(&event_app, &mut stdout);
                }
                CommandEvent::Stderr(bytes) => {
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    let _ = event_app.emit("app-server://stderr", text);
                }
                CommandEvent::Error(error) => {
                    let _ = event_app.emit("app-server://stderr", error);
                }
                CommandEvent::Terminated(payload) => {
                    if !stdout.trim().is_empty() {
                        let _ = event_app.emit("app-server://message", stdout.trim().to_string());
                    }
                    let _ = event_app.emit("app-server://exit", payload);
                    state.child = None;
                    break;
                }
                _ => {}
            }
        }
    });
    Ok(())
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
    process
        .process
        .write(&encoded)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn app_server_stop(state: State<'_, AppServerState>) -> Result<(), String> {
    if let Some(child) = state.inner.lock().await.child.take() {
        child.process.kill().map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn emit_complete_lines(app: &AppHandle, buffer: &mut String) {
    while let Some(newline) = buffer.find('\n') {
        let line = buffer[..newline].trim_end_matches('\r').to_string();
        buffer.drain(..=newline);
        if !line.is_empty() {
            let _ = app.emit("app-server://message", line);
        }
    }
}
