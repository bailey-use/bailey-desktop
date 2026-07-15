mod account;
mod cloud_records;
mod sidecar;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .manage(sidecar::AppServerState::default())
        .invoke_handler(tauri::generate_handler![
            account::bailey_account_status,
            account::bailey_account_clear_expired,
            account::bailey_account_login,
            account::bailey_account_register,
            account::bailey_account_logout,
            sidecar::app_server_start,
            sidecar::app_server_send,
            sidecar::app_server_stop,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Bailey desktop");
}
