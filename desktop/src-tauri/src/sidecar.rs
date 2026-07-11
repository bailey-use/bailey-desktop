use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
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
    if std::env::var_os("BAILEY_LOCAL_MCP_COMMAND").is_none()
        && let Some(path) = discover_local_tools_command(&app)
    {
        command = command.env("BAILEY_LOCAL_MCP_COMMAND", path);
    }
    if std::env::var_os("BAILEY_CUA_DRIVER_SOCKET").is_none()
        && std::env::var_os("BAILEY_CUA_DRIVER_COMMAND").is_none()
        && let Some(path) = discover_cua_driver(&app)
    {
        command = command.env("BAILEY_CUA_DRIVER_COMMAND", quote_command_path(&path));
    }
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
