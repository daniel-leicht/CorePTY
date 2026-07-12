// CorePTY backend entry point.
//
// Exposes session management (local PTY today; SSH and Telnet next) to the
// frontend over Tauri's IPC bridge. Terminal output streams back as
// `pty://data` / `pty://status` / `pty://exit` events.

#[cfg(windows)]
mod broker;
mod commands;
mod session;
mod store;

use session::SessionManager;

#[tauri::command]
fn ping() -> String {
    "pong".to_string()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Elevated re-launch: `corepty.exe --broker …` runs the broker, not the app.
    #[cfg(windows)]
    {
        let args: Vec<String> = std::env::args().collect();
        if args.iter().any(|a| a == "--broker") {
            broker::run(&args);
            return;
        }
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(SessionManager::default())
        .invoke_handler(tauri::generate_handler![
            ping,
            commands::session_create_local,
            commands::session_create_local_elevated,
            commands::session_create_ssh,
            commands::session_create_telnet,
            commands::session_write,
            commands::session_write_bytes,
            commands::session_resize,
            commands::session_close,
            commands::session_list,
            commands::list_local_shells,
            commands::host_os,
            store::secret_set,
            store::secret_get,
            store::secret_delete,
            store::sessions_load,
            store::sessions_upsert,
            store::sessions_delete,
            store::folders_load,
            store::folder_upsert,
            store::folder_delete,
            store::settings_load,
            store::settings_save,
        ])
        .run(tauri::generate_context!())
        .expect("error while running CorePTY");
}
