// CorePTY backend entry point.
//
// Exposes session management (local PTY today; SSH and Telnet next) to the
// frontend over Tauri's IPC bridge. Terminal output streams back as
// `pty://data` / `pty://status` / `pty://exit` events.

#[cfg(windows)]
mod broker;
mod commands;
mod connections;
mod session;
mod store;
mod transfer;
mod vfs;

use connections::ConnectionRegistry;
use session::SessionManager;
use std::sync::Arc;
use tauri::{Emitter, Manager};
use transfer::queue::TransferManager;
use vfs::FsRegistry;

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
        .setup(|app| {
            // A connection the server dropped is reported to the UI, which
            // marks the tabs and panels using it.
            let handle = app.handle().clone();
            app.state::<ConnectionRegistry>()
                .set_events(Arc::new(move |event| {
                    let _ = handle.emit("ssh://connection", event);
                }));
            Ok(())
        })
        .manage(SessionManager::default())
        .manage(ConnectionRegistry::default())
        .manage(FsRegistry::default())
        .manage(TransferManager::default())
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
            connections::ssh_connections,
            vfs::commands::fs_capabilities,
            vfs::commands::fs_home,
            vfs::commands::fs_roots,
            vfs::commands::fs_resolve,
            vfs::commands::fs_list,
            vfs::commands::fs_stat,
            vfs::commands::fs_mkdir,
            vfs::commands::fs_rename,
            vfs::commands::fs_delete,
            vfs::commands::fs_trash,
            vfs::commands::fs_open_native,
            vfs::commands::fs_read,
            vfs::commands::fs_dir_size,
            transfer::transfer_start,
            transfer::transfer_cancel,
            transfer::transfer_list,
            transfer::transfer_resolve,
            store::secret_set,
            store::secret_get,
            store::secret_delete,
            store::sessions_load,
            store::sessions_upsert,
            store::sessions_reorder,
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
