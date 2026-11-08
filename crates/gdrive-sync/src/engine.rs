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
use std::sync::atomic::{AtomicU64, Ordering};
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
use crate::state_db::{FileStateEntry, PendingUpload, StateDb};
use crate::sync_status::{FolderRun, StatusTracker};

/// Status/progress events the engine emits, consumed by the D-Bus service
/// layer in `gdrived` to report state to the Qt UI.
#[derive(Debug, Clone)]
pub enum SyncEvent {
    FolderStarted {
        folder_id: Uuid,
    },
    InitialSyncComplete {
        folder_id: Uuid,
        files_synced: usize,
    },
    LocalChangeUploaded {
        folder_id: Uuid,
        path: PathBuf,
    },
    RemoteChangeApplied {
        folder_id: Uuid,
        path: PathBuf,
    },
    Error {
        folder_id: Option<Uuid>,
        message: String,
    },
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
    status: StatusTracker,
    generation: Arc<AtomicU64>,
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
            status: StatusTracker::default(),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SyncEvent> {
        self.events.subscribe()
    }

    pub fn sync_status(&self) -> gdrive_common::dbus_api::SyncStatus {
        self.status.snapshot()
    }

    pub fn subscribe_status(&self) -> tokio::sync::watch::Receiver<u64> {
        self.status.subscribe()
    }

    /// Replaces a bare Drive API access token and discards refresh metadata.
    /// An empty token invalidates refreshes in flight. Does not start folders;
    /// OAuth sign-in and startup use save/restore to retain refresh metadata.
    pub fn set_access_token(&self, access_token: impl Into<String>) {
        self.drive.set_access_token(access_token);
    }

    /// Restores cached tokens, including refresh metadata, before starting sync.
    pub fn restore_token(&self, token: crate::auth::StoredToken, path: PathBuf) {
        self.drive.restore_token(token, path);
    }

    /// Saves a new session without allowing an older refresh to overwrite it.
    pub fn save_token(
        &self,
        token: crate::auth::StoredToken,
        path: &std::path::Path,
    ) -> Result<(), SyncError> {
        self.drive.save_token(token, path)
    }

    /// Lists the direct sub-folders of a Drive folder (`folder_id = "root"`
    /// for "My Drive"'s top level), for the "browse Drive folder" picker in
    /// the UI. Returns `(id, name)` pairs.
    pub async fn list_child_folders(
        &self,
        folder_id: &str,
    ) -> Result<Vec<(String, String)>, SyncError> {
        let folders = self.drive.list_child_folders(folder_id).await?;
        Ok(folders.into_iter().map(|f| (f.id, f.name)).collect())
    }

    /// Starts (or restarts) synchronisation for a single folder, spawning a
    /// background task that runs until [`Self::stop_folder`] is called or the
    /// task fails fatally. Restarting an already-running folder first aborts
    /// its previous task so there's never more than one watcher/poller per
    /// folder id.
    pub fn start_folder(&self, folder: SyncFolder) {
        let mut tasks = self.tasks.lock().unwrap();
        if let Some(previous) = tasks.remove(&folder.id) {
            previous.abort();
        }

        let state_db = Arc::clone(&self.state_db);
        let drive = Arc::clone(&self.drive);
        let events = self.events.clone();
        let poll_interval = Duration::from_secs(self.poll_interval_secs.max(5));
        let folder_id = folder.id;
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        let run = self.status.start(folder_id, generation);

        let handle = tokio::spawn(async move {
            if let Err(err) = run_folder(
                folder.clone(),
                state_db,
                drive,
                events.clone(),
                poll_interval,
                &run,
            )
            .await
            {
                run.finish("initial sync", Err(err.to_string()));
                let _ = events.send(SyncEvent::Error {
                    folder_id: Some(folder.id),
                    message: err.to_string(),
                });
            }
        });

        tasks.insert(folder_id, handle);
    }

    /// Stops the background task synchronising `folder_id`, if one is running.
    /// A no-op if the folder was never started or was already stopped.
    pub fn stop_folder(&self, folder_id: Uuid) {
        let mut tasks = self.tasks.lock().unwrap();
        if let Some(handle) = tasks.remove(&folder_id) {
            handle.abort();
        }
        self.status.stop(folder_id);
    }

    /// Returns the ids of all folders currently being synchronised.
    pub fn running_folders(&self) -> Vec<Uuid> {
        self.tasks
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, task)| !task.is_finished())
            .map(|(id, _)| *id)
            .collect()
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
    status: &FolderRun,
) -> Result<(), SyncError> {
    let _ = events.send(SyncEvent::FolderStarted {
        folder_id: folder.id,
    });

    tokio::fs::create_dir_all(&folder.local_path).await?;
    let (owner_uid, owner_gid) = resolve_owner(&folder.owner_user, &folder.owner_group);

    let root_folder_id = folder
        .drive_folder_id
        .clone()
        .unwrap_or_else(|| "root".to_string());

    // Retry Drive access itself rather than probing an unrelated internet host.
    let mut synced = 0;
    loop {
        status.active();
        let result = async {
            if state_db.get_page_token(folder.id)?.is_none() {
                let token = drive.get_start_page_token().await?;
                state_db.set_page_token(folder.id, &token)?;
            }
            retry_local_changes(
                &folder,
                &drive,
                &state_db,
                &root_folder_id,
                (owner_uid, owner_gid),
                &events,
                status,
            )
            .await?;
            initial_pull(
                &folder,
                &drive,
                &state_db,
                &root_folder_id,
                "",
                (owner_uid, owner_gid),
                &mut synced,
            )
            .await
        }
        .await;
        match result {
            Ok(()) => break,
            Err(error) if is_connection_failure(&error) => {
                status.finish("initial sync", Err(error.to_string()));
                let _ = events.send(SyncEvent::Error {
                    folder_id: Some(folder.id),
                    message: error.to_string(),
                });
                tokio::time::sleep(poll_interval).await;
            }
            Err(error) => return Err(error),
        }
    }
    let _ = events.send(SyncEvent::InitialSyncComplete {
        folder_id: folder.id,
        files_synced: synced,
    });

    // 2. Watch local changes and push them to Drive.
    let (local_tx, mut local_rx) = mpsc::unbounded_channel::<LocalChange>();
    let _watcher = LocalWatcher::watch(&folder.local_path, local_tx)?;
    status.finish("initial sync", Ok(()));

    let mut poll_timer = tokio::time::interval(poll_interval);
    poll_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some(change) = local_rx.recv() => {
                let result = async {
                    if let Ok(rel_path) = change.path.strip_prefix(&folder.local_path) {
                        if !rel_path.as_os_str().is_empty() {
                            state_db.queue_local_change(folder.id, &rel_path.to_string_lossy().replace('\\', "/"))?;
                        }
                    }
                    retry_local_changes(&folder, &drive, &state_db, &root_folder_id, (owner_uid, owner_gid), &events, status).await
                }.await;
                if let Err(err) = result {
                    status.finish("local retry queue", Err(err.to_string()));
                    let _ = events.send(SyncEvent::Error { folder_id: Some(folder.id), message: err.to_string() });
                }
            }
            _ = poll_timer.tick() => {
                if let Err(err) = retry_local_changes(&folder, &drive, &state_db, &root_folder_id, (owner_uid, owner_gid), &events, status).await {
                    status.finish("local retry queue", Err(err.to_string()));
                    let _ = events.send(SyncEvent::Error { folder_id: Some(folder.id), message: err.to_string() });
                }
                let result = poll_remote_changes(&folder, &drive, &state_db, owner_uid, owner_gid, &events, status).await;
                status.finish("remote changes", result.as_ref().map(|_| ()).map_err(ToString::to_string));
                if let Err(err) = result {
                    let _ = events.send(SyncEvent::Error { folder_id: Some(folder.id), message: err.to_string() });
                }
            }
        }
    }
}

async fn retry_local_changes(
    folder: &SyncFolder,
    drive: &DriveClient,
    state_db: &StateDb,
    root_folder_id: &str,
    owner: (nix::unistd::Uid, nix::unistd::Gid),
    events: &broadcast::Sender<SyncEvent>,
    status: &FolderRun,
) -> Result<(), SyncError> {
    for rel_path in state_db.pending_local_paths(folder.id)? {
        let change = LocalChange {
            path: folder.local_path.join(&rel_path),
            kind: crate::local_watcher::LocalChangeKind::Other,
        };
        let work = format!("local {rel_path}");
        let result = handle_local_change(
            folder,
            drive,
            state_db,
            root_folder_id,
            &change,
            owner.0,
            owner.1,
            status,
        )
        .await;
        match result {
            Ok(applied) => {
                state_db.complete_local_change(folder.id, &rel_path)?;
                status.finish(&work, Ok(()));
                if applied {
                    let _ = events.send(SyncEvent::LocalChangeUploaded {
                        folder_id: folder.id,
                        path: change.path,
                    });
                }
            }
            Err(error) => {
                status.finish(&work, Err(error.to_string()));
                let _ = events.send(SyncEvent::Error {
                    folder_id: Some(folder.id),
                    message: error.to_string(),
                });
            }
        }
    }
    status.clear_error("local retry queue");
    Ok(())
}

fn is_connection_failure(error: &SyncError) -> bool {
    let transport_failure = |error: &reqwest::Error| {
        error.is_connect() || error.is_timeout() || error.is_request() || error.is_body()
    };
    match error {
        SyncError::Http(error) => transport_failure(error),
        SyncError::HttpBody(_) => true,
        SyncError::OAuthTransport(crate::auth::ReqwestOAuthError::Reqwest(error)) => {
            transport_failure(error)
        }
        SyncError::OAuthTransport(crate::auth::ReqwestOAuthError::Body(_)) => true,
        SyncError::Timeout { .. } => true,
        _ => false,
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
    owner: (nix::unistd::Uid, nix::unistd::Gid),
    synced: &mut usize,
) -> Result<(), SyncError> {
    let (owner_uid, owner_gid) = owner;
    let children = drive.list_children(drive_folder_id).await?;

    for child in children {
        let rel_path = if local_rel_prefix.is_empty() {
            child.name.clone()
        } else {
            format!("{local_rel_prefix}/{}", child.name)
        };
        let local_path = folder.local_path.join(&rel_path);
        if state_db.has_pending_local_write(folder.id, &rel_path)? {
            continue;
        }

        if child.mime_type == FOLDER_MIME_TYPE {
            tokio::fs::create_dir_all(&local_path).await?;
            apply_ownership(&local_path, owner_uid, owner_gid)?;
            record_state(folder.id, state_db, &rel_path, &child, true)?;
            Box::pin(initial_pull(
                folder, drive, state_db, &child.id, &rel_path, owner, synced,
            ))
            .await?;
        } else {
            if let Some(existing) = state_db.find_by_drive_id(folder.id, &child.id)? {
                if local_file_changed(&local_path, &existing).await? {
                    state_db.queue_local_change(folder.id, &existing.local_rel_path)?;
                    continue;
                }
            }
            if !needs_download(state_db, folder.id, &rel_path, &child)? {
                continue;
            }
            let content = drive.download_file(&child.id).await?;
            if let Some(existing) = state_db.find_by_drive_id(folder.id, &child.id)? {
                if local_file_changed(&local_path, &existing).await? {
                    state_db.queue_local_change(folder.id, &existing.local_rel_path)?;
                    continue;
                }
            }
            tokio::fs::write(&local_path, &content).await?;
            apply_ownership(&local_path, owner_uid, owner_gid)?;
            record_state(folder.id, state_db, &rel_path, &child, false)?;
            *synced += 1;
        }
    }

    Ok(())
}

async fn local_file_changed(path: &Path, existing: &FileStateEntry) -> Result<bool, SyncError> {
    if existing.is_folder {
        return Ok(false);
    }
    match tokio::fs::read(path).await {
        Ok(content) => Ok(existing
            .md5_checksum
            .as_ref()
            .is_some_and(|checksum| checksum != &format!("{:x}", md5::compute(content)))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
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
    status: &FolderRun,
) -> Result<bool, SyncError> {
    let Ok(rel_path) = change.path.strip_prefix(&folder.local_path) else {
        return Ok(false);
    };
    let rel_path_str = rel_path.to_string_lossy().replace('\\', "/");
    if rel_path_str.is_empty() {
        return Ok(false);
    }

    let mut pending = state_db.pending_upload(folder.id, &rel_path_str)?;
    let mut completed_pending = false;
    if let Some(upload) = &mut pending {
        status.active();
        let uploaded = match &upload.uploaded {
            Some(file) => Some(file.clone()),
            None => drive.get_file(&upload.file_id).await?,
        };
        if let Some(file) = uploaded {
            upload.uploaded = Some(file);
            state_db.set_pending_upload(folder.id, &rel_path_str, Some(upload))?;
            finish_upload(folder, drive, state_db, &rel_path_str, upload).await?;
            pending = None;
            completed_pending = true;
        }
    }
    let metadata = match tokio::fs::metadata(&change.path).await {
        Ok(m) => m,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            state_db.set_pending_upload(folder.id, &rel_path_str, None)?;
            if let Some(existing) = find_local_state(state_db, folder.id, &rel_path_str)? {
                if let Some(drive_id) = existing.drive_file_id {
                    status.active();
                    drive.trash_file(&drive_id).await?;
                }
                state_db.remove_file_state(folder.id, &rel_path_str)?;
                return Ok(true);
            }
            status.clear_error(&format!("local {rel_path_str}"));
            return Ok(completed_pending);
        }
        Err(error) => return Err(error.into()),
    };

    let existing = find_local_state(state_db, folder.id, &rel_path_str)?;
    if metadata.is_dir() && existing.as_ref().is_some_and(|entry| entry.is_folder) {
        return Ok(completed_pending);
    }
    let content = if metadata.is_dir() {
        None
    } else {
        Some(tokio::fs::read(&change.path).await?)
    };
    if let Some(content) = &content {
        let checksum = format!("{:x}", md5::compute(content));
        if existing
            .as_ref()
            .and_then(|entry| entry.md5_checksum.as_deref())
            == Some(checksum.as_str())
        {
            return Ok(completed_pending);
        }
    }
    let name = rel_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let parent_drive_id = parent_drive_folder_id(folder, state_db, root_folder_id, rel_path)?;

    status.active();
    apply_ownership(&change.path, owner_uid, owner_gid)?;
    let mut upload = match pending {
        Some(upload) => upload,
        None => PendingUpload {
            file_id: drive.generate_file_id().await?,
            previous_drive_id: existing.and_then(|entry| entry.drive_file_id),
            is_folder: metadata.is_dir(),
            uploaded: None,
        },
    };
    upload.is_folder = metadata.is_dir();
    state_db.set_pending_upload(folder.id, &rel_path_str, Some(&upload))?;
    let uploaded = match content {
        Some(content) => {
            drive
                .upload_file(&upload.file_id, &name, &parent_drive_id, content)
                .await?
        }
        None => {
            drive
                .create_folder(&upload.file_id, &name, &parent_drive_id)
                .await?
        }
    };
    if uploaded.id != upload.file_id {
        return Err(SyncError::Api(
            "Drive returned an unexpected uploaded file ID".into(),
        ));
    }
    upload.uploaded = Some(uploaded);
    state_db.set_pending_upload(folder.id, &rel_path_str, Some(&upload))?;
    finish_upload(folder, drive, state_db, &rel_path_str, &upload).await?;

    Ok(true)
}

async fn finish_upload(
    folder: &SyncFolder,
    drive: &DriveClient,
    state_db: &StateDb,
    rel_path: &str,
    upload: &PendingUpload,
) -> Result<(), SyncError> {
    let uploaded = upload
        .uploaded
        .as_ref()
        .ok_or_else(|| SyncError::Api("pending upload has no completed file".into()))?;
    if uploaded.id != upload.file_id || uploaded.trashed {
        return Err(SyncError::Api(
            "pending replacement is unavailable or has an unexpected ID".into(),
        ));
    }
    if let Some(previous) = &upload.previous_drive_id {
        drive.trash_file(previous).await?;
    }
    record_state(folder.id, state_db, rel_path, uploaded, upload.is_folder)?;
    state_db.set_pending_upload(folder.id, rel_path, None)?;
    Ok(())
}

fn find_local_state(
    state_db: &StateDb,
    folder_id: Uuid,
    rel_path: &str,
) -> Result<Option<FileStateEntry>, SyncError> {
    state_db.find_by_local_path(folder_id, rel_path)
}

/// Looks up (or queues creation of) the Drive folder corresponding to the
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
                Some(entry) if entry.is_folder && entry.drive_file_id.is_some() => {
                    Ok(entry.drive_file_id.unwrap())
                }
                _ => {
                    state_db.queue_local_change(folder.id, &parent_rel_str)?;
                    Err(SyncError::Api(format!(
                        "local parent folder {parent_rel_str} has not synced yet"
                    )))
                }
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
    status: &FolderRun,
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
        if state_db.has_pending_local_change(folder.id, &existing.local_rel_path)? {
            continue;
        }
        if local_file_changed(&local_path, &existing).await? {
            state_db.queue_local_change(folder.id, &existing.local_rel_path)?;
            continue;
        }

        if change.removed || change.file.as_ref().map(|f| f.trashed).unwrap_or(true) {
            status.active();
            if local_path.exists() {
                if existing.is_folder {
                    tokio::fs::remove_dir_all(&local_path).await?;
                } else {
                    tokio::fs::remove_file(&local_path).await?;
                }
            }
            state_db.remove_file_state(folder.id, &existing.local_rel_path)?;
            continue;
        }

        if let Some(remote_file) = change.file {
            if remote_file.md5_checksum.as_deref() == existing.md5_checksum.as_deref() {
                continue; // Unchanged content, e.g. a metadata-only update.
            }
            status.active();
            let content = drive.download_file(&remote_file.id).await?;
            if local_file_changed(&local_path, &existing).await? {
                state_db.queue_local_change(folder.id, &existing.local_rel_path)?;
                continue;
            }
            tokio::fs::write(&local_path, &content).await?;
            apply_ownership(&local_path, owner_uid, owner_gid)?;
            record_state(
                folder.id,
                state_db,
                &existing.local_rel_path,
                &remote_file,
                existing.is_folder,
            )?;
            let _ = events.send(SyncEvent::RemoteChangeApplied {
                folder_id: folder.id,
                path: local_path,
            });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive_client::tests::{Reply, Server};

    const UPLOADED_NEW: &str = r#"{"id":"new-id","name":"file.txt","mimeType":"text/plain","md5Checksum":"22af645d1859cb5ca6da0c484f1f37ea"}"#;

    async fn local_fixture(server: &Server, content: &[u8]) -> (PathBuf, SyncFolder, SyncEngine) {
        let root = std::env::temp_dir().join(format!("gclient-local-retry-{}", Uuid::new_v4()));
        let folder = SyncFolder::new("fixture", root.join("files"), "", "");
        tokio::fs::create_dir_all(&folder.local_path).await.unwrap();
        tokio::fs::write(folder.local_path.join("file.txt"), content)
            .await
            .unwrap();
        let mut engine = SyncEngine::new(
            AppConfig::default(),
            StateDb::open(&root.join("state.sqlite")).unwrap(),
            String::new(),
        );
        engine.drive = Arc::new(server.client(Duration::from_secs(2), Duration::from_secs(2)));
        record_state(
            folder.id,
            &engine.state_db,
            "file.txt",
            &DriveFile {
                id: "old-id".into(),
                name: "file.txt".into(),
                mime_type: "text/plain".into(),
                md5_checksum: Some(format!("{:x}", md5::compute(b"old"))),
                parents: vec!["root".into()],
                modified_time: None,
                trashed: false,
                size: None,
            },
            false,
        )
        .unwrap();
        engine
            .state_db
            .queue_local_change(folder.id, "file.txt")
            .unwrap();
        (root, folder, engine)
    }

    async fn retry_fixture(folder: &SyncFolder, engine: &SyncEngine, status: &FolderRun) {
        retry_local_changes(
            folder,
            &engine.drive,
            &engine.state_db,
            "root",
            (nix::unistd::Uid::current(), nix::unistd::Gid::current()),
            &engine.events,
            status,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn failed_upload_retries_same_id_without_trashing_the_existing_copy() {
        let server = Server::start(vec![
            Reply::json(r#"{"ids":["new-id"]}"#),
            Reply::error(503),
            Reply::error(404),
            Reply::json(UPLOADED_NEW),
            Reply::json("{}"),
        ]);
        let (root, folder, mut engine) = local_fixture(&server, b"new").await;
        let status = engine.status.start(folder.id, 1);
        retry_fixture(&folder, &engine, &status).await;
        assert!(!engine.sync_status().folders[0].errors.is_empty());
        assert_eq!(
            engine
                .state_db
                .find_by_local_path(folder.id, "file.txt")
                .unwrap()
                .unwrap()
                .drive_file_id
                .as_deref(),
            Some("old-id")
        );
        assert!(engine
            .state_db
            .pending_upload(folder.id, "file.txt")
            .unwrap()
            .unwrap()
            .uploaded
            .is_none());
        assert_eq!(server.requests.try_iter().count(), 2);

        engine.state_db = Arc::new(StateDb::open(&root.join("state.sqlite")).unwrap());
        retry_fixture(&folder, &engine, &status).await;
        assert!(engine
            .state_db
            .pending_local_paths(folder.id)
            .unwrap()
            .is_empty());
        assert!(engine.sync_status().folders[0].errors.is_empty());
        assert_eq!(
            engine
                .state_db
                .find_by_local_path(folder.id, "file.txt")
                .unwrap()
                .unwrap()
                .drive_file_id
                .as_deref(),
            Some("new-id")
        );
        let requests: Vec<_> = server.requests.try_iter().collect();
        assert!(requests[0].starts_with("GET /files/new-id?"));
        assert!(requests[1].starts_with("POST /files?"));
        assert!(requests[1].contains(r#""id":"new-id""#));
        assert!(requests[2].starts_with("PATCH /files/old-id "));
        assert!(requests[2].contains(r#""trashed":true"#));
        drop(status);
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn failed_trash_resumes_uploaded_replacement_after_restart() {
        let server = Server::start(vec![
            Reply::json(r#"{"ids":["new-id"]}"#),
            Reply::json(UPLOADED_NEW),
            Reply::error(503),
            Reply::json("{}"),
        ]);
        let (root, folder, mut engine) = local_fixture(&server, b"new").await;
        let status = engine.status.start(folder.id, 1);
        retry_fixture(&folder, &engine, &status).await;
        assert!(engine
            .state_db
            .pending_upload(folder.id, "file.txt")
            .unwrap()
            .unwrap()
            .uploaded
            .is_some());
        let requests: Vec<_> = server.requests.try_iter().collect();
        assert!(requests[1].starts_with("POST /files?"));
        assert!(requests[2].starts_with("PATCH /files/old-id "));
        engine.state_db = Arc::new(StateDb::open(&root.join("state.sqlite")).unwrap());
        retry_fixture(&folder, &engine, &status).await;
        let requests: Vec<_> = server.requests.try_iter().collect();
        assert_eq!(requests.len(), 1, "retry must not create another copy");
        assert!(requests[0].starts_with("PATCH /files/old-id "));
        assert!(engine
            .state_db
            .pending_local_paths(folder.id)
            .unwrap()
            .is_empty());
        assert!(engine.sync_status().folders[0].errors.is_empty());
        drop(status);
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn lost_upload_response_recovers_created_id_instead_of_uploading_again() {
        let server = Server::start(vec![
            Reply::json(r#"{"ids":["new-id"]}"#),
            Reply::truncated(UPLOADED_NEW),
            Reply::json(UPLOADED_NEW),
            Reply::json("{}"),
        ]);
        let (root, folder, engine) = local_fixture(&server, b"new").await;
        let status = engine.status.start(folder.id, 1);
        retry_fixture(&folder, &engine, &status).await;
        assert!(!engine.sync_status().folders[0].errors.is_empty());
        retry_fixture(&folder, &engine, &status).await;
        let requests: Vec<_> = server.requests.try_iter().collect();
        assert_eq!(
            requests.iter().filter(|r| r.starts_with("POST ")).count(),
            1
        );
        assert!(requests[2].starts_with("GET /files/new-id?"));
        assert!(requests[3].starts_with("PATCH /files/old-id "));
        assert!(engine
            .state_db
            .pending_local_paths(folder.id)
            .unwrap()
            .is_empty());
        drop(status);
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn local_deletion_is_trashed_and_failed_trash_is_retried() {
        let server = Server::start(vec![Reply::error(503), Reply::json("{}")]);
        let (root, folder, engine) = local_fixture(&server, b"old").await;
        tokio::fs::remove_file(folder.local_path.join("file.txt"))
            .await
            .unwrap();
        let status = engine.status.start(folder.id, 1);
        retry_fixture(&folder, &engine, &status).await;
        assert!(engine
            .state_db
            .find_by_local_path(folder.id, "file.txt")
            .unwrap()
            .is_some());
        retry_fixture(&folder, &engine, &status).await;
        assert!(engine
            .state_db
            .find_by_local_path(folder.id, "file.txt")
            .unwrap()
            .is_none());
        assert!(engine
            .state_db
            .pending_local_paths(folder.id)
            .unwrap()
            .is_empty());
        assert!(engine.sync_status().folders[0].errors.is_empty());
        let requests: Vec<_> = server.requests.try_iter().collect();
        assert_eq!(requests.len(), 2);
        assert!(requests
            .iter()
            .all(|r| r.starts_with("PATCH /files/old-id ") && r.contains(r#""trashed":true"#)));
        drop(status);
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn remote_writes_and_deletions_do_not_echo_back_to_drive() {
        let server = Server::start(vec![
            Reply::json(
                r#"{"changes":[{"fileId":"old-id","removed":false,"file":{"id":"old-id","name":"file.txt","mimeType":"text/plain","md5Checksum":"22af645d1859cb5ca6da0c484f1f37ea"}}],"newStartPageToken":"updated"}"#,
            ),
            Reply::json("new"),
            Reply::json(
                r#"{"changes":[{"fileId":"old-id","removed":true}],"newStartPageToken":"removed"}"#,
            ),
        ]);
        let (root, folder, engine) = local_fixture(&server, b"old").await;
        let status = engine.status.start(folder.id, 1);
        retry_fixture(&folder, &engine, &status).await;
        engine.state_db.set_page_token(folder.id, "start").unwrap();
        for expected in ["updated", "removed"] {
            poll_remote_changes(
                &folder,
                &engine.drive,
                &engine.state_db,
                nix::unistd::Uid::current(),
                nix::unistd::Gid::current(),
                &engine.events,
                &status,
            )
            .await
            .unwrap();
            engine
                .state_db
                .queue_local_change(folder.id, "file.txt")
                .unwrap();
            retry_fixture(&folder, &engine, &status).await;
            assert_eq!(
                engine
                    .state_db
                    .get_page_token(folder.id)
                    .unwrap()
                    .as_deref(),
                Some(expected)
            );
        }
        let requests: Vec<_> = server.requests.try_iter().collect();
        assert_eq!(requests.len(), 3);
        assert!(requests.iter().all(|r| r.starts_with("GET ")));
        assert!(engine.sync_status().folders[0].errors.is_empty());
        drop(status);
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn local_edit_during_remote_download_is_preserved_and_queued() {
        let mut download = Reply::json("new");
        download.header_delay = Duration::from_millis(200);
        let server = Server::start(vec![
            Reply::json(
                r#"{"changes":[{"fileId":"old-id","removed":false,"file":{"id":"old-id","name":"file.txt","mimeType":"text/plain","md5Checksum":"22af645d1859cb5ca6da0c484f1f37ea"}}],"newStartPageToken":"updated"}"#,
            ),
            download,
        ]);
        let (root, folder, engine) = local_fixture(&server, b"old").await;
        engine
            .state_db
            .complete_local_change(folder.id, "file.txt")
            .unwrap();
        engine.state_db.set_page_token(folder.id, "start").unwrap();
        let status = engine.status.start(folder.id, 1);
        let (result, ()) = tokio::join!(
            poll_remote_changes(
                &folder,
                &engine.drive,
                &engine.state_db,
                nix::unistd::Uid::current(),
                nix::unistd::Gid::current(),
                &engine.events,
                &status
            ),
            async {
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        if server
                            .requests
                            .try_iter()
                            .any(|request| request.starts_with("GET /files/old-id?"))
                        {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                tokio::fs::write(folder.local_path.join("file.txt"), b"edited locally")
                    .await
                    .unwrap();
            }
        );
        result.unwrap();
        assert_eq!(
            tokio::fs::read(folder.local_path.join("file.txt"))
                .await
                .unwrap(),
            b"edited locally"
        );
        assert_eq!(
            engine.state_db.pending_local_paths(folder.id).unwrap(),
            vec!["file.txt"]
        );
        drop(status);
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn missing_local_parent_is_queued_and_created_before_retrying_child() {
        let server = Server::start(vec![
            Reply::json(r#"{"ids":["parent-id"]}"#),
            Reply::json(
                r#"{"id":"parent-id","name":"dir","mimeType":"application/vnd.google-apps.folder"}"#,
            ),
            Reply::json(r#"{"ids":["new-id"]}"#),
            Reply::json(UPLOADED_NEW),
        ]);
        let (root, folder, engine) = local_fixture(&server, b"old").await;
        engine
            .state_db
            .complete_local_change(folder.id, "file.txt")
            .unwrap();
        tokio::fs::create_dir(folder.local_path.join("dir"))
            .await
            .unwrap();
        tokio::fs::write(folder.local_path.join("dir/file.txt"), b"new")
            .await
            .unwrap();
        engine
            .state_db
            .queue_local_change(folder.id, "dir/file.txt")
            .unwrap();
        let status = engine.status.start(folder.id, 1);
        retry_fixture(&folder, &engine, &status).await;
        assert_eq!(
            engine.state_db.pending_local_paths(folder.id).unwrap(),
            vec!["dir", "dir/file.txt"]
        );
        assert!(server.requests.try_iter().next().is_none());
        retry_fixture(&folder, &engine, &status).await;
        assert!(engine
            .state_db
            .pending_local_paths(folder.id)
            .unwrap()
            .is_empty());
        assert!(engine.sync_status().folders[0].errors.is_empty());
        let requests: Vec<_> = server.requests.try_iter().collect();
        assert!(requests[1].starts_with("POST /files?"));
        assert!(requests[3].contains(r#""parents":["parent-id"]"#));
        drop(status);
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn worker_retries_persisted_local_upload_without_another_filesystem_event() {
        let server = Server::start(vec![
            Reply::json(r#"{"startPageToken":"start"}"#),
            Reply::json(r#"{"ids":["new-id"]}"#),
            Reply::error(503),
            Reply::json(
                r#"{"files":[{"id":"old-id","name":"file.txt","mimeType":"text/plain","md5Checksum":"149603e6c03516362a8da23f624db945"}]}"#,
            ),
            Reply::error(404),
            Reply::json(UPLOADED_NEW),
            Reply::json("{}"),
            Reply::json(r#"{"changes":[],"newStartPageToken":"polled"}"#),
        ]);
        let (root, folder, engine) = local_fixture(&server, b"new").await;
        let mut events = engine.subscribe();
        engine.start_folder(folder.clone());
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                if matches!(
                    events.recv().await.unwrap(),
                    SyncEvent::LocalChangeUploaded { .. }
                ) {
                    break;
                }
            }
            while engine
                .state_db
                .get_page_token(folder.id)
                .unwrap()
                .as_deref()
                != Some("polled")
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        engine.stop_folder(folder.id);
        tokio::task::yield_now().await;
        assert!(engine
            .state_db
            .pending_local_paths(folder.id)
            .unwrap()
            .is_empty());
        assert_eq!(
            tokio::fs::read(folder.local_path.join("file.txt"))
                .await
                .unwrap(),
            b"new"
        );
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn initial_connection_timeout_retries_and_downloads_after_recovery() {
        let mut stalled = Reply::json(r#"{"files":[]}"#);
        stalled.header_delay = Duration::from_millis(750);
        let server = Server::start(vec![
            stalled,
            Reply::json(r#"{"startPageToken":"recovered"}"#),
            Reply::json(
                r#"{"files":[{"id":"remote-file","name":"file.txt","mimeType":"text/plain","md5Checksum":"5aa922017b17cac6405dc17ad2880ec0"}]}"#,
            ),
            Reply::json("updated while offline"),
            Reply::json(r#"{"changes":[],"newStartPageToken":"polled"}"#),
        ]);
        let root = std::env::temp_dir().join(format!("gclient-recovery-{}", Uuid::new_v4()));
        let folder = SyncFolder::new("fixture", root.join("files"), "", "");
        let mut engine = SyncEngine::new(
            AppConfig::default(),
            StateDb::open(&root.join("state.sqlite")).unwrap(),
            String::new(),
        );
        engine.poll_interval_secs = 0;
        engine.drive = Arc::new(server.client(Duration::from_millis(250), Duration::from_secs(2)));
        let mut events = engine.subscribe();
        engine.start_folder(folder.clone());

        tokio::time::timeout(Duration::from_secs(2), async {
            while !matches!(events.recv().await.unwrap(), SyncEvent::Error { .. }) {}
        })
        .await
        .unwrap();
        let snapshot = engine.sync_status();
        assert!(!snapshot.folders[0].syncing);
        assert!(!snapshot.folders[0].errors.is_empty());
        assert_eq!(engine.running_folders(), vec![folder.id]);
        assert!(!folder.local_path.join("file.txt").exists());

        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if let SyncEvent::InitialSyncComplete { files_synced, .. } =
                    events.recv().await.unwrap()
                {
                    assert_eq!(files_synced, 1);
                    break;
                }
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while engine
                .state_db
                .get_page_token(folder.id)
                .unwrap()
                .as_deref()
                != Some("polled")
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            engine.sync_status().folders[0].errors.is_empty(),
            "{:?}",
            engine.sync_status()
        );
        engine.stop_folder(folder.id);
        tokio::task::yield_now().await;
        assert_eq!(
            tokio::fs::read(folder.local_path.join("file.txt"))
                .await
                .unwrap(),
            b"updated while offline"
        );
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn partial_pull_counts_downloads_and_replays_deletions_during_retry() {
        let server = Server::start(vec![
            Reply::json(r#"{"startPageToken":"before-pull"}"#),
            Reply::json(
                r#"{"files":[{"id":"a","name":"deleted.txt","mimeType":"text/plain","md5Checksum":"a989fcd914f3f5ba28e59b49a51a558a"},{"id":"b","name":"retained.txt","mimeType":"text/plain","md5Checksum":"97976ee534ef5442764ab8c6b436a3ab"}]}"#,
            ),
            Reply::json("downloaded before interruption"),
            Reply::truncated("interrupted download"),
            Reply::json(
                r#"{"files":[{"id":"b","name":"retained.txt","mimeType":"text/plain","md5Checksum":"97976ee534ef5442764ab8c6b436a3ab"}]}"#,
            ),
            Reply::json("downloaded after recovery"),
            Reply::json(
                r#"{"changes":[{"fileId":"a","removed":true}],"newStartPageToken":"polled"}"#,
            ),
        ]);
        let root =
            std::env::temp_dir().join(format!("gclient-partial-recovery-{}", Uuid::new_v4()));
        let folder = SyncFolder::new("fixture", root.join("files"), "", "");
        let mut engine = SyncEngine::new(
            AppConfig::default(),
            StateDb::open(&root.join("state.sqlite")).unwrap(),
            String::new(),
        );
        engine.poll_interval_secs = 0;
        engine.drive = Arc::new(server.client(Duration::from_secs(1), Duration::from_secs(2)));
        let mut events = engine.subscribe();
        engine.start_folder(folder.clone());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !matches!(events.recv().await.unwrap(), SyncEvent::Error { .. }) {}
        })
        .await
        .unwrap();
        assert!(folder.local_path.join("deleted.txt").exists());
        assert!(!folder.local_path.join("retained.txt").exists());
        assert_eq!(
            engine
                .state_db
                .get_page_token(folder.id)
                .unwrap()
                .as_deref(),
            Some("before-pull")
        );
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if let SyncEvent::InitialSyncComplete { files_synced, .. } =
                    events.recv().await.unwrap()
                {
                    assert_eq!(files_synced, 2);
                    break;
                }
            }
            while engine
                .state_db
                .get_page_token(folder.id)
                .unwrap()
                .as_deref()
                != Some("polled")
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!folder.local_path.join("deleted.txt").exists());
        assert!(engine
            .state_db
            .find_by_drive_id(folder.id, "a")
            .unwrap()
            .is_none());
        assert!(engine.sync_status().folders[0].errors.is_empty());
        engine.stop_folder(folder.id);
        tokio::task::yield_now().await;
        assert_eq!(
            tokio::fs::read(folder.local_path.join("retained.txt"))
                .await
                .unwrap(),
            b"downloaded after recovery"
        );
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn stopping_folder_cancels_initial_connection_retry() {
        let mut stalled = Reply::json(r#"{"files":[]}"#);
        stalled.header_delay = Duration::from_millis(750);
        let server = Server::start(vec![stalled]);
        let root = std::env::temp_dir().join(format!("gclient-offline-stop-{}", Uuid::new_v4()));
        let folder = SyncFolder::new("fixture", root.join("files"), "", "");
        let mut engine = SyncEngine::new(
            AppConfig::default(),
            StateDb::open(&root.join("state.sqlite")).unwrap(),
            String::new(),
        );
        engine.drive = Arc::new(server.client(Duration::from_millis(250), Duration::from_secs(2)));
        let mut events = engine.subscribe();
        engine.start_folder(folder.clone());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !matches!(events.recv().await.unwrap(), SyncEvent::Error { .. }) {}
        })
        .await
        .unwrap();
        let task = engine
            .tasks
            .lock()
            .unwrap()
            .get(&folder.id)
            .unwrap()
            .abort_handle();
        engine.stop_folder(folder.id);
        tokio::task::yield_now().await;
        assert!(task.is_finished());
        assert!(engine.running_folders().is_empty());
        assert!(engine.sync_status().folders.is_empty());
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn malformed_initial_listing_is_fatal_not_a_connection_retry() {
        let server = Server::start(vec![
            Reply::json(r#"{"startPageToken":"before-pull"}"#),
            Reply::json(r#"{"unexpected":[]}"#),
        ]);
        let root = std::env::temp_dir().join(format!("gclient-invalid-list-{}", Uuid::new_v4()));
        let folder = SyncFolder::new("fixture", root.join("files"), "", "");
        let mut engine = SyncEngine::new(
            AppConfig::default(),
            StateDb::open(&root.join("state.sqlite")).unwrap(),
            String::new(),
        );
        engine.drive = Arc::new(server.client(Duration::from_secs(1), Duration::from_secs(2)));
        let mut events = engine.subscribe();
        engine.start_folder(folder.clone());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !matches!(events.recv().await.unwrap(), SyncEvent::Error { .. }) {}
            while !engine.running_folders().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!engine.sync_status().folders[0].syncing);
        assert!(!engine.sync_status().folders[0].errors.is_empty());
        engine.stop_folder(folder.id);
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn fatal_initial_failure_is_reported_and_stop_restart_clear_it() {
        let root = std::env::temp_dir().join(format!("gclient-status-{}", Uuid::new_v4()));
        tokio::fs::create_dir(&root).await.unwrap();
        let path = root.join("not-a-directory");
        tokio::fs::write(&path, b"fixture").await.unwrap();
        let folder = SyncFolder::new("fixture", path, "", "");
        let engine = SyncEngine::new(
            AppConfig::default(),
            StateDb::open(&root.join("state.sqlite")).unwrap(),
            String::new(),
        );
        let mut changes = engine.subscribe_status();
        engine.start_folder(folder.clone());
        assert!(engine.sync_status().folders[0].syncing);
        tokio::time::timeout(Duration::from_secs(2), async {
            while engine.sync_status().folders[0].errors.is_empty() {
                changes.changed().await.unwrap();
            }
            while !engine.running_folders().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let status = engine.sync_status();
        assert!(!status.folders[0].syncing);
        assert!(status.folders[0].errors[0].contains("initial sync"));
        engine.start_folder(folder.clone());
        assert!(engine.sync_status().folders[0].errors.is_empty());
        engine.stop_folder(folder.id);
        tokio::task::yield_now().await;
        assert!(engine.sync_status().folders.is_empty());
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn vanished_rename_sources_clear_only_untracked_work_errors() {
        let root = std::env::temp_dir().join(format!("gclient-rename-{}", Uuid::new_v4()));
        let db = StateDb::open(&root.join("state.sqlite")).unwrap();
        let folder = SyncFolder::new("fixture", root.join("files"), "", "");
        tokio::fs::create_dir(&folder.local_path).await.unwrap();
        let drive = DriveClient::new(String::new());

        for (name, tracked) in [("untracked.txt", false), ("tracked.txt", true)] {
            let source = folder.local_path.join(name);
            tokio::fs::write(&source, b"fixture").await.unwrap();
            if tracked {
                db.upsert_file_state(&FileStateEntry {
                    sync_folder_id: folder.id,
                    local_rel_path: name.into(),
                    drive_file_id: Some("remote-file".into()),
                    is_folder: false,
                    local_mtime_secs: None,
                    drive_modified_time: None,
                    md5_checksum: None,
                })
                .unwrap();
            }
            let tracker = StatusTracker::default();
            let status = tracker.start(folder.id, 1);
            let work = format!("local {name}");
            status.finish(&work, Err("upload failed".into()));
            status.finish("local unrelated.txt", Err("another failure".into()));
            let renamed = format!("renamed-{name}");
            tokio::fs::rename(&source, folder.local_path.join(&renamed))
                .await
                .unwrap();
            let change = LocalChange {
                path: source,
                kind: crate::local_watcher::LocalChangeKind::Modified,
            };
            let result = handle_local_change(
                &folder,
                &drive,
                &db,
                "root",
                &change,
                nix::unistd::Uid::current(),
                nix::unistd::Gid::current(),
                &status,
            )
            .await;
            if tracked {
                assert!(matches!(result, Err(SyncError::NotAuthenticated)));
            } else {
                assert!(!result.unwrap());
            }
            status.finish(&format!("local {renamed}"), Ok(()));
            let snapshot = tracker.snapshot();
            let errors = &snapshot.folders[0].errors;
            assert_eq!(
                errors.iter().any(|error| error.starts_with(&work)),
                tracked,
                "a vanished source only retains work with a pending remote deletion"
            );
            assert!(errors
                .iter()
                .any(|error| error.starts_with("local unrelated.txt:")));
            assert!(!snapshot.folders[0].syncing);
        }

        drop(db);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn empty_poll_and_disappeared_local_file_never_start_activity() {
        let root = std::env::temp_dir().join(format!("gclient-idle-{}", Uuid::new_v4()));
        let db = StateDb::open(&root.join("state.sqlite")).unwrap();
        let folder = SyncFolder::new("fixture", root.join("files"), "", "");
        let drive = DriveClient::new(String::new());
        let tracker = StatusTracker::default();
        let status = tracker.start(folder.id, 1);
        status.finish("initial sync", Ok(()));
        let before = tracker.snapshot();
        let (events, _) = broadcast::channel(1);
        poll_remote_changes(
            &folder,
            &drive,
            &db,
            nix::unistd::Uid::current(),
            nix::unistd::Gid::current(),
            &events,
            &status,
        )
        .await
        .unwrap();
        let change = LocalChange {
            path: folder.local_path.join("vanished"),
            kind: crate::local_watcher::LocalChangeKind::Modified,
        };
        assert!(!handle_local_change(
            &folder,
            &drive,
            &db,
            "root",
            &change,
            nix::unistd::Uid::current(),
            nix::unistd::Gid::current(),
            &status,
        )
        .await
        .unwrap());
        assert_eq!(tracker.snapshot(), before);
        drop(db);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
