//! Background copy/move jobs between any two registered filesystems.

pub mod engine;
pub mod queue;

use crate::vfs::FsRegistry;
use engine::ConflictAnswer;
use queue::{JobView, TransferEvent, TransferManager, TransferRequest};
use std::sync::Arc;
use tauri::{ipc::Channel, State};

/// Queue a copy or move. Returns at once; progress, conflict questions and the
/// outcome arrive on `on_event`.
#[tauri::command]
pub fn transfer_start(
    manager: State<'_, TransferManager>,
    registry: State<'_, FsRegistry>,
    request: TransferRequest,
    on_event: Channel<TransferEvent>,
) -> Result<JobView, String> {
    let source_fs = request
        .sources
        .first()
        .map(|location| location.fs.clone())
        .ok_or_else(|| "select at least one file or folder".to_string())?;
    let source = registry.get(&source_fs)?;
    let target = registry.get(&request.destination.fs)?;
    manager.start(
        request,
        source,
        target,
        Arc::new(move |event| {
            let _ = on_event.send(event);
        }),
    )
}

#[tauri::command]
pub fn transfer_cancel(manager: State<'_, TransferManager>, id: String) -> bool {
    manager.cancel(&id)
}

#[tauri::command]
pub fn transfer_list(manager: State<'_, TransferManager>) -> Vec<JobView> {
    manager.list()
}

/// Answer the conflict question `conflict_id` of job `id`.
#[tauri::command]
pub fn transfer_resolve(
    manager: State<'_, TransferManager>,
    id: String,
    conflict_id: u64,
    answer: ConflictAnswer,
) -> Result<(), String> {
    manager.resolve(&id, conflict_id, answer)
}
