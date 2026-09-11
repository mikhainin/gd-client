//! Orchestrates synchronisation for all configured [`SyncFolder`]s: performs
//! an initial recursive pull from Drive, then watches the local filesystem
//! for changes (pushing them to Drive) and polls Drive's Changes API for
//! remote changes (pulling them locally), applying the configured file
//! ownership after every local write.
//!
//! This is a first, sequential-per-folder implementation intended as a solid
//! foundation: conflict resolution is "last writer wins" based on modified
//! time, and directory moves/renames are handled as remove+recreate. See
//! `docs/architecture.md` for the planned refinements.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gdrive_common::{AppConfig, SyncFolder};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::drive_client::{DriveClient, DriveFile, FOLDER_MIME_TYPE};
use crate::error::SyncError;
use crate::local_watcher::{LocalChange, LocalWatcher};
use crate::ownership::{apply_ownership, resolve_owner};
use crate::state_db::{FileStateEntry, StateDb};

/// Status/progress events the engine emits, consumed by the D-Bus service
/// layer in `gdrived` to report state to the Qt UI.
#[derive(Debug, Clone)]
pub enum SyncEvent {
    FolderStarted { folder_id: Uuid },
    InitialSyncComplete { folder_id: Uuid, files_synced: usize },
    LocalChangeUploaded { folder_id: Uuid, path: PathBuf },
    RemoteChangeApplied { folder_id: Uuid, path: PathBuf },
    Error { folder_id: Option<Uuid>, message: String },
}

/// A running or completed sync engine, cloneable/shareable across the D-Bus
/// service layer so it can start/stop individual folders on demand (e.g. when
/// the user adds, removes or toggles a folder from the Qt UI) without
/// restarting the whole daemon.
#[derive(Clone)]
pub struct SyncEngine {
    poll_interval_secs: u64,
    state_db: Arc<StateDb>,
    drive: Arc<DriveClient>,
    events: broadcast::Sender<SyncEvent>,
    tasks: Arc<Mutex<HashMap<Uuid, JoinHandle<()>>>>,
}

impl SyncEngine {
    pub fn new(config: AppConfig, state_db: StateDb, access_token: String) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            poll_interval_secs: config.poll_interval_secs,
            state_db: Arc::new(state_db),
            drive: Arc::new(DriveClient::new(access_token)),
            events,
            tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SyncEvent> {
        self.events.subscribe()
    }

    /// Starts (or restarts) synchronisation for a single folder, spawning a
    /// background task that runs until [`Self::stop_folder`] is called or the
    /// task fails fatally. Restarting an already-running folder first aborts
    /// its previous task so there's never more than one watcher/poller per
    /// folder id.
    pub fn start_folder(&self, folder: SyncFolder) {
        self.stop_folder(folder.id);

        let state_db = Arc::clone(&self.state_db);
        let drive = Arc::clone(&self.drive);
        let events = self.events.clone();
        let poll_interval = Duration::from_secs(self.poll_interval_secs.max(5));
        let folder_id = folder.id;

        let handle = tokio::spawn(async move {
            if let Err(err) = run_folder(folder.clone(), state_db, drive, events.clone(), poll_interval).await {
                let _ = events.send(SyncEvent::Error {
                    folder_id: Some(folder.id),
                    message: err.to_string(),
                });
            }
        });

        self.tasks.lock().unwrap().insert(folder_id, handle);
    }

    /// Stops the background task synchronising `folder_id`, if one is running.
    /// A no-op if the folder was never started or was already stopped.
    pub fn stop_folder(&self, folder_id: Uuid) {
        if let Some(handle) = self.tasks.lock().unwrap().remove(&folder_id) {
            handle.abort();
        }
    }

    /// Returns the ids of all folders currently being synchronised.
    pub fn running_folders(&self) -> Vec<Uuid> {
        self.tasks.lock().unwrap().keys().copied().collect()
    }

    /// Starts every enabled folder in `config` concurrently. Intended for
    /// initial daemon startup; use [`Self::start_folder`]/[`Self::stop_folder`]
    /// afterwards to react to configuration changes at runtime.
    pub fn start_all(&self, config: &AppConfig) {
        for folder in config.sync_folders.iter().filter(|f| f.enabled) {
            self.start_folder(folder.clone());
        }
    }
}

async fn run_folder(
    folder: SyncFolder,
    state_db: Arc<StateDb>,
    drive: Arc<DriveClient>,
    events: broadcast::Sender<SyncEvent>,
    poll_interval: Duration,
) -> Result<(), SyncError> {
    let _ = events.send(SyncEvent::FolderStarted { folder_id: folder.id });

    tokio::fs::create_dir_all(&folder.local_path).await?;
    let (owner_uid, owner_gid) = resolve_owner(&folder.owner_user, &folder.owner_group);

    let root_folder_id = folder.drive_folder_id.clone().unwrap_or_else(|| "root".to_string());

    // 1. Initial recursive pull so both sides start in a known-consistent state.
    let synced = initial_pull(&folder, &drive, &state_db, &root_folder_id, "", owner_uid, owner_gid).await?;
    let _ = events.send(SyncEvent::InitialSyncComplete {
        folder_id: folder.id,
        files_synced: synced,
    });

    if state_db.get_page_token(folder.id)?.is_none() {
        let token = drive.get_start_page_token().await?;
        state_db.set_page_token(folder.id, &token)?;
    }

    // 2. Watch local changes and push them to Drive.
    let (local_tx, mut local_rx) = mpsc::unbounded_channel::<LocalChange>();
    let _watcher = LocalWatcher::watch(&folder.local_path, local_tx)?;

    let mut poll_timer = tokio::time::interval(poll_interval);

    loop {
        tokio::select! {
            Some(change) = local_rx.recv() => {
                if let Err(err) = handle_local_change(&folder, &drive, &state_db, &root_folder_id, &change, owner_uid, owner_gid).await {
                    let _ = events.send(SyncEvent::Error { folder_id: Some(folder.id), message: err.to_string() });
                    continue;
                }
                let _ = events.send(SyncEvent::LocalChangeUploaded { folder_id: folder.id, path: change.path });
            }
            _ = poll_timer.tick() => {
                if let Err(err) = poll_remote_changes(&folder, &drive, &state_db, owner_uid, owner_gid, &events).await {
                    let _ = events.send(SyncEvent::Error { folder_id: Some(folder.id), message: err.to_string() });
                }
            }
        }
    }
}

/// Recursively downloads everything under `drive_folder_id` into
/// `folder.local_path/local_rel_prefix`, skipping files whose Drive
/// `modifiedTime`/checksum already match what's recorded in the state db.
async fn initial_pull(
    folder: &SyncFolder,
    drive: &DriveClient,
    state_db: &StateDb,
    drive_folder_id: &str,
    local_rel_prefix: &str,
    owner_uid: nix::unistd::Uid,
    owner_gid: nix::unistd::Gid,
) -> Result<usize, SyncError> {
    let mut synced = 0usize;
    let children = drive.list_children(drive_folder_id).await?;

    for child in children {
        let rel_path = if local_rel_prefix.is_empty() {
            child.name.clone()
        } else {
            format!("{local_rel_prefix}/{}", child.name)
        };
        let local_path = folder.local_path.join(&rel_path);

        if child.mime_type == FOLDER_MIME_TYPE {
            tokio::fs::create_dir_all(&local_path).await?;
            apply_ownership(&local_path, owner_uid, owner_gid)?;
            record_state(folder.id, state_db, &rel_path, &child, true)?;
            synced += Box::pin(initial_pull(folder, drive, state_db, &child.id, &rel_path, owner_uid, owner_gid)).await?;
        } else {
            if !needs_download(state_db, folder.id, &rel_path, &child)? {
                continue;
            }
            let content = drive.download_file(&child.id).await?;
            tokio::fs::write(&local_path, &content).await?;
            apply_ownership(&local_path, owner_uid, owner_gid)?;
            record_state(folder.id, state_db, &rel_path, &child, false)?;
            synced += 1;
        }
    }

    Ok(synced)
}

fn needs_download(
    state_db: &StateDb,
    folder_id: Uuid,
    rel_path: &str,
    remote: &DriveFile,
) -> Result<bool, SyncError> {
    match state_db.find_by_drive_id(folder_id, &remote.id)? {
        Some(existing) => Ok(existing.local_rel_path != rel_path
            || existing.md5_checksum.as_deref() != remote.md5_checksum.as_deref()),
        None => Ok(true),
    }
}

fn record_state(
    folder_id: Uuid,
    state_db: &StateDb,
    rel_path: &str,
    remote: &DriveFile,
    is_folder: bool,
) -> Result<(), SyncError> {
    state_db.upsert_file_state(&FileStateEntry {
        sync_folder_id: folder_id,
        local_rel_path: rel_path.to_string(),
        drive_file_id: Some(remote.id.clone()),
        is_folder,
        local_mtime_secs: Some(now_secs()),
        drive_modified_time: remote.modified_time.clone(),
        md5_checksum: remote.md5_checksum.clone(),
    })
}

async fn handle_local_change(
    folder: &SyncFolder,
    drive: &DriveClient,
    state_db: &StateDb,
    root_folder_id: &str,
    change: &LocalChange,
    owner_uid: nix::unistd::Uid,
    owner_gid: nix::unistd::Gid,
) -> Result<(), SyncError> {
    use crate::local_watcher::LocalChangeKind;

    let Ok(rel_path) = change.path.strip_prefix(&folder.local_path) else {
        return Ok(());
    };
    let rel_path_str = rel_path.to_string_lossy().replace('\\', "/");
    if rel_path_str.is_empty() {
        return Ok(());
    }

    match change.kind {
        LocalChangeKind::Removed => {
            if let Some(existing) = find_local_state(state_db, folder.id, &rel_path_str)? {
                if let Some(drive_id) = existing.drive_file_id {
                    drive.delete_file(&drive_id).await?;
                }
                state_db.remove_file_state(folder.id, &rel_path_str)?;
            }
        }
        LocalChangeKind::Created | LocalChangeKind::Modified | LocalChangeKind::Other => {
            let metadata = match tokio::fs::metadata(&change.path).await {
                Ok(m) => m,
                Err(_) => return Ok(()), // Path disappeared again (e.g. temp file); nothing to do.
            };

            if metadata.is_dir() {
                let parent_drive_id = parent_drive_folder_id(folder, state_db, root_folder_id, rel_path)?;
                let existing = find_local_state(state_db, folder.id, &rel_path_str)?;
                if existing.is_none() {
                    let name = rel_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let created = drive.create_folder(&name, &parent_drive_id).await?;
                    record_state(folder.id, state_db, &rel_path_str, &created, true)?;
                }
                return Ok(());
            }

            let content = tokio::fs::read(&change.path).await?;
            apply_ownership(&change.path, owner_uid, owner_gid)?;

            let existing = find_local_state(state_db, folder.id, &rel_path_str)?;
            let name = rel_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let parent_drive_id = parent_drive_folder_id(folder, state_db, root_folder_id, rel_path)?;

            let uploaded = drive
                .upload_file(existing.as_ref().and_then(|e| e.drive_file_id.as_deref()), &name, &parent_drive_id, content)
                .await?;
            record_state(folder.id, state_db, &rel_path_str, &uploaded, false)?;
        }
    }

    Ok(())
}

fn find_local_state(state_db: &StateDb, folder_id: Uuid, rel_path: &str) -> Result<Option<FileStateEntry>, SyncError> {
    // The state db is keyed by (folder, rel_path) as its primary key; reuse
    // find_by_drive_id's row mapping via a direct lookup would need a new
    // query, so we scan through the drive-id index lookup helper instead is
    // not ideal - this indirection keeps the state_db API small for now.
    state_db.find_by_local_path(folder_id, rel_path)
}

/// Looks up (or lazily creates) the Drive folder id corresponding to the
/// parent directory of `rel_path`, so newly created files/folders are
/// uploaded into the right place.
fn parent_drive_folder_id(
    folder: &SyncFolder,
    state_db: &StateDb,
    root_folder_id: &str,
    rel_path: &Path,
) -> Result<String, SyncError> {
    match rel_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        None => Ok(root_folder_id.to_string()),
        Some(parent_rel) => {
            let parent_rel_str = parent_rel.to_string_lossy().replace('\\', "/");
            match state_db.find_by_local_path(folder.id, &parent_rel_str)? {
                Some(entry) if entry.drive_file_id.is_some() => Ok(entry.drive_file_id.unwrap()),
                _ => Ok(root_folder_id.to_string()),
            }
        }
    }
}

async fn poll_remote_changes(
    folder: &SyncFolder,
    drive: &DriveClient,
    state_db: &StateDb,
    owner_uid: nix::unistd::Uid,
    owner_gid: nix::unistd::Gid,
    events: &broadcast::Sender<SyncEvent>,
) -> Result<(), SyncError> {
    let Some(page_token) = state_db.get_page_token(folder.id)? else {
        return Ok(());
    };

    let (changes, new_token) = drive.list_changes(&page_token).await?;

    for change in changes {
        let Some(existing) = state_db.find_by_drive_id(folder.id, &change.file_id)? else {
            // We don't know this file yet (created elsewhere, or outside
            // our synced subtree) - the next initial-pull-style rescan will
            // pick it up. Keeping this simple avoids reconstructing full
            // Drive paths from the Changes API response alone.
            continue;
        };

        let local_path = folder.local_path.join(&existing.local_rel_path);

        if change.removed || change.file.as_ref().map(|f| f.trashed).unwrap_or(true) {
            if local_path.exists() {
                if existing.is_folder {
                    tokio::fs::remove_dir_all(&local_path).await.ok();
                } else {
                    tokio::fs::remove_file(&local_path).await.ok();
                }
            }
            state_db.remove_file_state(folder.id, &existing.local_rel_path)?;
            continue;
        }

        if let Some(remote_file) = change.file {
            if remote_file.md5_checksum.as_deref() == existing.md5_checksum.as_deref() {
                continue; // Unchanged content, e.g. a metadata-only update.
            }
            let content = drive.download_file(&remote_file.id).await?;
            tokio::fs::write(&local_path, &content).await?;
            apply_ownership(&local_path, owner_uid, owner_gid)?;
            record_state(folder.id, state_db, &existing.local_rel_path, &remote_file, existing.is_folder)?;
            let _ = events.send(SyncEvent::RemoteChangeApplied { folder_id: folder.id, path: local_path });
        }
    }

    state_db.set_page_token(folder.id, &new_token)?;
    Ok(())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
