use serde_json::Value;
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

    let command = app
        .shell()
        .sidecar("aivo-app-server")
        .map_err(|error| error.to_string())?
        .args(["app-server", "--stdio"]);
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
