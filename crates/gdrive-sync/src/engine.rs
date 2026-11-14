//! Orchestrates synchronisation for all configured [`SyncFolder`]s: performs
//! an initial recursive pull from Drive, then watches the local filesystem
//! for changes (pushing them to Drive) and polls Drive's Changes API for
//! remote changes (pulling them locally), applying the configured file
//! ownership after every local write.
//!
//! Transfers are sequential within each folder. Live Drive ancestry is used
//! to discover new items and reconcile remote moves, including across restarts.

use std::collections::{HashMap, HashSet};
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
use crate::local_watcher::{is_download_staging, path_key, LocalChange, LocalWatcher};
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

    let (local_tx, mut local_rx) = mpsc::unbounded_channel::<Result<LocalChange, SyncError>>();
    let _watcher = LocalWatcher::watch(
        &folder.local_path,
        folder.id,
        Arc::clone(&state_db),
        local_tx,
    )?;
    scan_local_changes(&folder, &state_db).await?;

    // Retry Drive access itself rather than probing an unrelated internet host.
    let mut synced = 0;
    loop {
        status.active();
        let result = async {
            if state_db.get_page_token(folder.id)?.is_none() {
                let token = drive.get_start_page_token().await?;
                state_db.set_page_token(folder.id, &token)?;
            }
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

    status.finish("initial sync", Ok(()));

    let mut poll_timer = tokio::time::interval(poll_interval);
    poll_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some(change) = local_rx.recv() => {
                let result = match change {
                    Ok(_) => sync_local_changes(&folder, &drive, &state_db, &root_folder_id, (owner_uid, owner_gid), &events, status).await,
                    Err(error) => {
                        status.finish("local watcher", Err(error.to_string()));
                        let _ = events.send(SyncEvent::Error { folder_id: Some(folder.id), message: error.to_string() });
                        continue;
                    },
                };
                if let Err(err) = result {
                    status.finish("local retry queue", Err(err.to_string()));
                    let _ = events.send(SyncEvent::Error { folder_id: Some(folder.id), message: err.to_string() });
                }
            }
            _ = poll_timer.tick() => {
                if let Err(err) = sync_local_changes(&folder, &drive, &state_db, &root_folder_id, (owner_uid, owner_gid), &events, status).await {
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

async fn scan_local_changes(folder: &SyncFolder, state_db: &StateDb) -> Result<(), SyncError> {
    queue_dirty_descendants(folder, state_db, &folder.local_path).await?;
    for existing in state_db.file_states(folder.id)? {
        if !tokio::fs::try_exists(folder.local_path.join(&existing.local_rel_path)).await? {
            state_db.queue_local_change(folder.id, &existing.local_rel_path)?;
        }
    }
    Ok(())
}

async fn sync_local_changes(
    folder: &SyncFolder,
    drive: &DriveClient,
    state_db: &StateDb,
    root_folder_id: &str,
    owner: (nix::unistd::Uid, nix::unistd::Gid),
    events: &broadcast::Sender<SyncEvent>,
    status: &FolderRun,
) -> Result<(), SyncError> {
    scan_local_changes(folder, state_db).await?;
    let mut untracked = false;
    for key in state_db.pending_local_paths(folder.id)? {
        if state_db.find_by_local_path(folder.id, &key)?.is_none()
            && tokio::fs::try_exists(folder.local_path.join(&key)).await?
        {
            untracked = true;
            break;
        }
    }
    if untracked {
        status.active();
        let mut synced = 0;
        initial_pull(
            folder,
            drive,
            state_db,
            root_folder_id,
            "",
            owner,
            &mut synced,
        )
        .await?;
    }
    for key in state_db.pending_local_paths(folder.id)? {
        let Some(existing) = state_db.find_by_local_path(folder.id, &key)? else {
            continue;
        };
        if existing.is_folder
            || tokio::fs::try_exists(folder.local_path.join(&key)).await?
            || !local_file_changed(&folder.local_path.join(&key), &existing).await?
        {
            continue;
        }
        let Some(id) = &existing.drive_file_id else {
            continue;
        };
        let Some(remote) = drive.get_file(id).await? else {
            continue;
        };
        if !remote.trashed && remote_content_changed(&existing, &remote) {
            status.active();
            let content = drive.download_file(id).await?;
            save_conflict(folder, state_db, &key, &content, "remote").await?;
            // Advance the comparison baseline only after the remote version is safe.
            record_state(folder.id, state_db, &key, &remote, false)?;
        }
    }
    retry_local_changes(
        folder,
        drive,
        state_db,
        root_folder_id,
        owner,
        events,
        status,
    )
    .await
}

fn remote_content_changed(existing: &FileStateEntry, remote: &DriveFile) -> bool {
    existing.md5_checksum != remote.md5_checksum
        || (remote.md5_checksum.is_none() && existing.drive_modified_time != remote.modified_time)
}

async fn save_conflict(
    folder: &SyncFolder,
    state_db: &StateDb,
    key: &str,
    content: &[u8],
    side: &str,
) -> Result<String, SyncError> {
    let path = Path::new(key);
    let name = path
        .file_name()
        .ok_or_else(|| SyncError::Api("conflict has no filename".into()))?;
    let digest = format!("{:x}", md5::compute(content));
    let mut conflict = path.with_file_name(format!(
        "{}.{}-conflict-{}",
        path_key(Path::new(name))?,
        side,
        digest
    ));
    if tokio::fs::try_exists(folder.local_path.join(&conflict)).await? {
        if tokio::fs::read(folder.local_path.join(&conflict)).await? == content {
            let key = path_key(&conflict)?;
            state_db.queue_local_change(folder.id, &key)?;
            return Ok(key);
        }
        conflict = path.with_file_name(format!(
            "{}.{}-conflict-{}",
            path_key(Path::new(name))?,
            side,
            Uuid::new_v4()
        ));
    }
    let key = path_key(&conflict)?;
    atomic_download(&folder.local_path.join(&conflict), content).await?;
    let (uid, gid) = resolve_owner(&folder.owner_user, &folder.owner_group);
    apply_ownership(&folder.local_path.join(&conflict), uid, gid)?;
    state_db.queue_local_change(folder.id, &key)?;
    Ok(key)
}

async fn preserve_local_path(
    folder: &SyncFolder,
    state_db: &StateDb,
    key: &str,
) -> Result<String, SyncError> {
    let path = Path::new(key);
    let name = path
        .file_name()
        .ok_or_else(|| SyncError::Api("conflict has no filename".into()))?;
    let conflict = path.with_file_name(format!(
        "{}.local-conflict-{}",
        path_key(Path::new(name))?,
        Uuid::new_v4()
    ));
    let conflict_key = path_key(&conflict)?;
    tokio::fs::rename(
        folder.local_path.join(path),
        folder.local_path.join(&conflict),
    )
    .await?;
    state_db.queue_local_change(folder.id, &conflict_key)?;
    if tokio::fs::metadata(folder.local_path.join(&conflict))
        .await?
        .is_dir()
    {
        queue_dirty_descendants(folder, state_db, &folder.local_path.join(&conflict)).await?;
    }
    Ok(conflict_key)
}

async fn atomic_download(path: &Path, content: &[u8]) -> Result<(), SyncError> {
    use tokio::io::AsyncWriteExt;
    let parent = path
        .parent()
        .ok_or_else(|| SyncError::Api("download has no parent".into()))?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = parent.join(format!(".gdrive-download-{}", Uuid::new_v4()));
    let result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await?;
        file.write_all(content).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary, path).await?;
        Ok::<(), SyncError>(())
    }
    .await;
    if result.is_err() {
        if let Err(error) = tokio::fs::remove_file(&temporary).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("could not remove incomplete download: {error}");
            }
        }
    }
    result
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
    let mut paths = Vec::new();
    for key in state_db.pending_local_paths(folder.id)? {
        paths.push((
            !tokio::fs::try_exists(folder.local_path.join(&key)).await?,
            key,
        ));
    }
    paths.sort();
    let mut failed_write = false;
    for (missing, rel_path) in paths {
        if missing && failed_write {
            continue;
        }
        let Some(revision) = state_db.pending_local_revision(folder.id, &rel_path)? else {
            continue;
        };
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
                let existing = state_db.find_by_local_path(folder.id, &rel_path)?;
                let changed_again = match existing {
                    Some(existing) => local_file_changed(&change.path, &existing).await?,
                    None => tokio::fs::try_exists(&change.path).await?,
                };
                if !changed_again {
                    state_db.complete_local_revision(folder.id, &rel_path, revision)?;
                }
                status.finish(&work, Ok(()));
                if applied {
                    let _ = events.send(SyncEvent::LocalChangeUploaded {
                        folder_id: folder.id,
                        path: change.path,
                    });
                }
            }
            Err(error) => {
                failed_write |= !missing;
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
        validate_remote_name(&child.name)?;
        let rel_path = if local_rel_prefix.is_empty() {
            child.name.clone()
        } else {
            format!("{local_rel_prefix}/{}", child.name)
        };
        let local_path = folder.local_path.join(&rel_path);
        if let Some(at_path) = state_db.find_by_local_path(folder.id, &rel_path)? {
            if at_path.drive_file_id.as_deref() != Some(&child.id) {
                let live = match &at_path.drive_file_id {
                    Some(id) => drive.get_file(id).await?,
                    None => None,
                };
                let live_path = match live.as_ref().filter(|file| !file.trashed) {
                    Some(file) => remote_path(folder, drive, file).await?,
                    None => None,
                };
                if let Some(chain) = live_path {
                    let mut destination = PathBuf::new();
                    for item in chain {
                        validate_remote_name(&item.name)?;
                        destination.push(&item.name);
                    }
                    let destination = path_key(&destination)?;
                    if destination == rel_path {
                        return Err(SyncError::Api(
                            "multiple Drive items map to the same local path".into(),
                        ));
                    }
                    if !relocate_remote_entry(
                        folder,
                        state_db,
                        live.as_ref().unwrap(),
                        &destination,
                    )
                    .await?
                    {
                        continue;
                    }
                } else {
                    if local_file_changed(&local_path, &at_path).await?
                        || (at_path.is_folder
                            && queue_dirty_descendants(folder, state_db, &local_path).await?)
                    {
                        if tokio::fs::try_exists(&local_path).await? {
                            preserve_local_path(folder, state_db, &rel_path).await?;
                        }
                    } else if at_path.is_folder {
                        tokio::fs::remove_dir_all(&local_path).await?;
                    } else {
                        tokio::fs::remove_file(&local_path).await?;
                    }
                    state_db.remove_file_state(folder.id, &rel_path)?;
                }
            }
        }
        if !relocate_remote_entry(folder, state_db, &child, &rel_path).await? {
            continue;
        }
        if state_db.pending_upload(folder.id, &rel_path)?.is_some() {
            continue;
        }

        if child.mime_type == FOLDER_MIME_TYPE {
            if tokio::fs::try_exists(&local_path).await?
                && !tokio::fs::metadata(&local_path).await?.is_dir()
            {
                preserve_local_path(folder, state_db, &rel_path).await?;
            }
            if let Some(existing) = state_db.find_by_drive_id(folder.id, &child.id)? {
                if local_file_changed(&local_path, &existing).await? {
                    state_db.queue_local_change(folder.id, &existing.local_rel_path)?;
                    continue;
                }
            }
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
                    if remote_content_changed(&existing, &child) && existing.md5_checksum.is_some()
                    {
                        let content = drive.download_file(&child.id).await?;
                        save_conflict(folder, state_db, &rel_path, &content, "remote").await?;
                        record_state(folder.id, state_db, &rel_path, &child, false)?;
                    }
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
            } else if tokio::fs::try_exists(&local_path).await? {
                preserve_local_path(folder, state_db, &rel_path).await?;
            }
            atomic_download(&local_path, &content).await?;
            apply_ownership(&local_path, owner_uid, owner_gid)?;
            record_state(folder.id, state_db, &rel_path, &child, false)?;
            *synced += 1;
        }
    }

    Ok(())
}

fn validate_remote_name(name: &str) -> Result<(), SyncError> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(SyncError::Api(
            "Drive name cannot be represented safely as a local filename".into(),
        ));
    }
    Ok(())
}

async fn relocate_remote_entry(
    folder: &SyncFolder,
    state_db: &StateDb,
    remote: &DriveFile,
    rel_path: &str,
) -> Result<bool, SyncError> {
    let Some(existing) = state_db.find_by_drive_id(folder.id, &remote.id)? else {
        return Ok(true);
    };
    if existing.local_rel_path == rel_path {
        return Ok(true);
    }
    let old = folder.local_path.join(&existing.local_rel_path);
    if state_db.has_pending_local_change(folder.id, &existing.local_rel_path)?
        || state_db.has_pending_local_change(folder.id, rel_path)?
        || local_file_changed(&old, &existing).await?
        || (existing.is_folder && queue_dirty_descendants(folder, state_db, &old).await?)
    {
        state_db.queue_local_change(folder.id, &existing.local_rel_path)?;
        return Ok(false);
    }
    let new = folder.local_path.join(rel_path);
    if tokio::fs::try_exists(&new).await? {
        if state_db.find_by_local_path(folder.id, rel_path)?.is_some() {
            return Err(SyncError::Api(
                "remote move targets another tracked Drive item".into(),
            ));
        }
        preserve_local_path(folder, state_db, rel_path).await?;
    }
    if let Some(parent) = new.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::rename(&old, &new).await?;
    state_db.relocate_file_state(folder.id, &existing.local_rel_path, rel_path)?;
    Ok(true)
}

/// Resolve live ancestry rather than trusting stale directory mappings after moves.
async fn remote_path(
    folder: &SyncFolder,
    drive: &DriveClient,
    file: &DriveFile,
) -> Result<Option<Vec<DriveFile>>, SyncError> {
    let mut root_id = folder
        .drive_folder_id
        .as_deref()
        .unwrap_or("root")
        .to_owned();
    let mut chain = vec![file.clone()];
    let mut visited = HashSet::new();
    for _ in 0..256 {
        let current = chain.last().unwrap();
        if current.id == root_id {
            chain.pop();
            chain.reverse();
            return Ok(Some(chain));
        }
        if !visited.insert(current.id.clone()) {
            return Err(SyncError::Api("cycle in Drive parent hierarchy".into()));
        }
        let Some(parent) = current.parents.first() else {
            return Ok(None);
        };
        if parent == &root_id {
            chain.reverse();
            return Ok(Some(chain));
        }
        if root_id == "root" {
            root_id = drive
                .get_file("root")
                .await?
                .ok_or_else(|| SyncError::Api("Drive root is unavailable".into()))?
                .id;
            if parent == &root_id {
                chain.reverse();
                return Ok(Some(chain));
            }
        }
        let Some(parent_file) = drive.get_file(parent).await? else {
            return Ok(None);
        };
        if parent_file.trashed {
            return Ok(None);
        }
        chain.push(parent_file);
    }
    Err(SyncError::Api("Drive hierarchy exceeds 256 levels".into()))
}

async fn local_file_changed(path: &Path, existing: &FileStateEntry) -> Result<bool, SyncError> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir() != existing.is_folder {
        return Ok(true);
    }
    if existing.is_folder {
        return Ok(false);
    }
    match tokio::fs::read(path).await {
        Ok(content) => Ok(existing
            .md5_checksum
            .as_ref()
            .is_none_or(|checksum| checksum != &format!("{:x}", md5::compute(content)))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

async fn queue_dirty_descendants(
    folder: &SyncFolder,
    state_db: &StateDb,
    directory: &Path,
) -> Result<bool, SyncError> {
    let mut directories = vec![directory.to_path_buf()];
    let mut dirty = false;
    while let Some(directory) = directories.pop() {
        let mut entries = match tokio::fs::read_dir(&directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if is_download_staging(&path) {
                continue;
            }
            let relative = path.strip_prefix(&folder.local_path)?;
            let key = path_key(relative)?;
            let existing = state_db.find_by_local_path(folder.id, &key)?;
            let changed = match existing {
                Some(existing) => local_file_changed(&path, &existing).await?,
                None => true,
            };
            if changed {
                state_db.queue_local_change(folder.id, &key)?;
                dirty = true;
            }
            if entry.file_type().await?.is_dir() {
                directories.push(path);
            }
        }
    }
    Ok(dirty)
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
    let rel_path_str = path_key(rel_path)?;
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
        .map(|name| path_key(Path::new(name)))
        .transpose()?
        .ok_or_else(|| SyncError::Api("local file has no name".into()))?;
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
    let uploaded = drive.get_file(&upload.file_id).await?;
    let Some(uploaded) = uploaded.filter(|file| !file.trashed) else {
        state_db.set_pending_upload(folder.id, rel_path, None)?;
        return Err(SyncError::Api(
            "replacement is missing or trashed; previous Drive copy was preserved and a new upload will be retried".into(),
        ));
    };
    if uploaded.id != upload.file_id {
        return Err(SyncError::Api(
            "pending replacement is unavailable or has an unexpected ID".into(),
        ));
    }
    if let Some(previous) = &upload.previous_drive_id {
        if let Some(existing) = state_db.find_by_local_path(folder.id, rel_path)? {
            if !existing.is_folder {
                if let Some(remote) = drive.get_file(previous).await? {
                    if !remote.trashed && remote_content_changed(&existing, &remote) {
                        let content = drive.download_file(previous).await?;
                        save_conflict(folder, state_db, rel_path, &content, "remote").await?;
                        record_state(folder.id, state_db, rel_path, &remote, false)?;
                    }
                }
            }
        }
        drive.trash_file(previous).await?;
    }
    record_state(folder.id, state_db, rel_path, &uploaded, upload.is_folder)?;
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
            let parent_rel_str = path_key(parent_rel)?;
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
    let mut classified = Vec::new();
    for change in changes {
        let resolved = match change
            .file
            .as_ref()
            .filter(|file| !change.removed && !file.trashed && !file.parents.is_empty())
        {
            Some(file) => Some(remote_path(folder, drive, file).await?),
            None => None,
        };
        classified.push((change, resolved));
    }
    // Moves out of the root release paths just like deletions do.
    classified.sort_by_key(|(change, resolved)| {
        !(change.removed
            || change.file.as_ref().is_none_or(|file| file.trashed)
            || matches!(resolved, Some(None)))
    });

    let mut deferred = false;
    for (change, resolved) in classified {
        let mut existing = state_db.find_by_drive_id(folder.id, &change.file_id)?;
        let outside = matches!(&resolved, Some(None));
        if let Some(remote) = change
            .file
            .as_ref()
            .filter(|file| !file.trashed && !change.removed)
        {
            if let Some(Some(chain)) = resolved {
                let mut rel_path = PathBuf::new();
                let mut blocked = false;
                for item in &chain {
                    validate_remote_name(&item.name)?;
                    rel_path.push(&item.name);
                    let key = path_key(&rel_path)?;
                    if state_db.has_pending_local_change(folder.id, &key)?
                        || !relocate_remote_entry(folder, state_db, item, &key).await?
                    {
                        deferred = true;
                        blocked = true;
                        break;
                    }
                    if let Some(at_path) = state_db.find_by_local_path(folder.id, &key)? {
                        if at_path.drive_file_id.as_deref() != Some(&item.id) {
                            return Err(SyncError::Api(
                                "multiple Drive items map to the same local path".into(),
                            ));
                        }
                    }
                    if item.mime_type == FOLDER_MIME_TYPE {
                        let path = folder.local_path.join(&rel_path);
                        if state_db.find_by_drive_id(folder.id, &item.id)?.is_none()
                            && tokio::fs::try_exists(&path).await?
                            && !tokio::fs::metadata(&path).await?.is_dir()
                        {
                            preserve_local_path(folder, state_db, &key).await?;
                        }
                        tokio::fs::create_dir_all(folder.local_path.join(&rel_path)).await?;
                        apply_ownership(&folder.local_path.join(&rel_path), owner_uid, owner_gid)?;
                        record_state(folder.id, state_db, &key, item, true)?;
                    }
                }
                if blocked {
                    continue;
                }
                if chain.is_empty() {
                    continue;
                }
                existing = state_db.find_by_drive_id(folder.id, &change.file_id)?;
                if remote.mime_type == FOLDER_MIME_TYPE {
                    status.active();
                    let mut synced = 0;
                    initial_pull(
                        folder,
                        drive,
                        state_db,
                        &remote.id,
                        &path_key(&rel_path)?,
                        (owner_uid, owner_gid),
                        &mut synced,
                    )
                    .await?;
                    continue;
                }
                if existing.is_none() {
                    status.active();
                    let content = drive.download_file(&remote.id).await?;
                    let path = folder.local_path.join(&rel_path);
                    if state_db.has_pending_local_change(folder.id, &path_key(&rel_path)?)? {
                        deferred = true;
                        continue;
                    }
                    if tokio::fs::try_exists(&path).await? {
                        preserve_local_path(folder, state_db, &path_key(&rel_path)?).await?;
                    }
                    atomic_download(&path, &content).await?;
                    apply_ownership(&path, owner_uid, owner_gid)?;
                    record_state(folder.id, state_db, &path_key(&rel_path)?, remote, false)?;
                    let _ = events.send(SyncEvent::RemoteChangeApplied {
                        folder_id: folder.id,
                        path,
                    });
                    continue;
                }
            }
        }
        let Some(existing) = existing else { continue };

        let local_path = folder.local_path.join(&existing.local_rel_path);
        let deleted =
            outside || change.removed || change.file.as_ref().is_none_or(|file| file.trashed);
        if deleted && tokio::fs::try_exists(&local_path).await? {
            let dirty = local_file_changed(&local_path, &existing).await?
                || (existing.is_folder
                    && queue_dirty_descendants(folder, state_db, &local_path).await?);
            if dirty {
                status.active();
                preserve_local_path(folder, state_db, &existing.local_rel_path).await?;
                state_db.remove_file_state(folder.id, &existing.local_rel_path)?;
                continue;
            }
        }
        if state_db.has_pending_local_change(folder.id, &existing.local_rel_path)? {
            deferred = true;
            continue;
        }
        if local_file_changed(&local_path, &existing).await? {
            state_db.queue_local_change(folder.id, &existing.local_rel_path)?;
            deferred = true;
            continue;
        }
        if existing.is_folder {
            let dirty = queue_dirty_descendants(folder, state_db, &local_path).await?;
            if dirty || state_db.has_pending_local_change(folder.id, &existing.local_rel_path)? {
                deferred = true;
                continue;
            }
        }

        if deleted {
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
                deferred = true;
                continue;
            }
            atomic_download(&local_path, &content).await?;
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

    if !deferred {
        state_db.set_page_token(folder.id, &new_token)?;
    }
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
    const REMOTE_OLD: &str = r#"{"id":"old-id","name":"file.txt","mimeType":"text/plain","md5Checksum":"149603e6c03516362a8da23f624db945"}"#;

    #[tokio::test]
    async fn concurrent_remote_edit_is_saved_before_trash_and_is_not_duplicated_on_retry() {
        const EDIT: &str = r#"{"id":"old-id","name":"file.txt","mimeType":"text/plain","md5Checksum":"changed-remotely"}"#;
        let server = Server::start(vec![
            Reply::json(r#"{"ids":["new-id"]}"#),
            Reply::json(UPLOADED_NEW),
            Reply::json(UPLOADED_NEW),
            Reply::json(EDIT),
            Reply::json("remote edit"),
            Reply::error(503),
            Reply::json(UPLOADED_NEW),
            Reply::json(EDIT),
            Reply::json("{}"),
        ]);
        let (root, folder, engine) = local_fixture(&server, b"new").await;
        let status = engine.status.start(folder.id, 1);
        retry_fixture(&folder, &engine, &status).await;
        let conflict = format!(
            "file.txt.remote-conflict-{:x}",
            md5::compute(b"remote edit")
        );
        assert_eq!(
            tokio::fs::read(folder.local_path.join(&conflict))
                .await
                .unwrap(),
            b"remote edit"
        );
        let requests: Vec<_> = server.requests.try_iter().collect();
        assert!(requests[4].contains("alt=media"));
        assert!(requests[5].starts_with("PATCH /files/old-id "));
        // Finish only the staged retirement; the conflict copy remains queued for upload.
        let change = LocalChange {
            path: folder.local_path.join("file.txt"),
            kind: crate::local_watcher::LocalChangeKind::Modified,
        };
        handle_local_change(
            &folder,
            &engine.drive,
            &engine.state_db,
            "root",
            &change,
            nix::unistd::Uid::current(),
            nix::unistd::Gid::current(),
            &status,
        )
        .await
        .unwrap();
        assert_eq!(server.requests.try_iter().count(), 3);
        assert!(engine
            .state_db
            .pending_local_paths(folder.id)
            .unwrap()
            .contains(&conflict));
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn initial_pull_preserves_preexisting_and_in_flight_local_content_and_queues_local_only_files(
    ) {
        let mut download = Reply::json("remote");
        download.header_delay = Duration::from_millis(150);
        let server = Server::start(vec![
            Reply::json(r#"{"startPageToken":"start"}"#),
            Reply::json(
                r#"{"files":[{"id":"old-id","name":"file.txt","mimeType":"text/plain","md5Checksum":"remote"}]}"#,
            ),
            download,
        ]);
        let (root, folder, engine) = local_fixture(&server, b"preexisting").await;
        engine
            .state_db
            .remove_file_state(folder.id, "file.txt")
            .unwrap();
        tokio::fs::create_dir(folder.local_path.join("local-only"))
            .await
            .unwrap();
        tokio::fs::write(folder.local_path.join("local-only/child.txt"), b"child")
            .await
            .unwrap();
        let mut events = engine.subscribe();
        engine.start_folder(folder.clone());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if server
                    .requests
                    .try_iter()
                    .any(|request| request.contains("alt=media"))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::fs::write(
            folder.local_path.join("file.txt"),
            b"edited during download",
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    events.recv().await.unwrap(),
                    SyncEvent::InitialSyncComplete { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        engine.stop_folder(folder.id);
        tokio::task::yield_now().await;
        assert_eq!(
            tokio::fs::read(folder.local_path.join("file.txt"))
                .await
                .unwrap(),
            b"remote"
        );
        let pending = engine.state_db.pending_local_paths(folder.id).unwrap();
        assert!(pending.contains(&"local-only/child.txt".into()));
        let conflict = pending
            .iter()
            .find(|key| key.starts_with("file.txt.local-conflict-"))
            .unwrap();
        assert_eq!(
            tokio::fs::read(folder.local_path.join(conflict))
                .await
                .unwrap(),
            b"edited during download"
        );
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn local_directory_rename_is_scanned_recursively_and_failed_upload_preserves_remote_source(
    ) {
        let server = Server::start(vec![
            Reply::json(r#"{"ids":["new-dir"]}"#),
            Reply::error(503),
        ]);
        let (root, folder, engine) = local_fixture(&server, b"old").await;
        engine
            .state_db
            .complete_local_change(folder.id, "file.txt")
            .unwrap();
        tokio::fs::create_dir(folder.local_path.join("renamed"))
            .await
            .unwrap();
        tokio::fs::rename(
            folder.local_path.join("file.txt"),
            folder.local_path.join("renamed/file.txt"),
        )
        .await
        .unwrap();
        scan_local_changes(&folder, &engine.state_db).await.unwrap();
        let pending = engine.state_db.pending_local_paths(folder.id).unwrap();
        assert!(pending.contains(&"renamed".into()));
        assert!(pending.contains(&"renamed/file.txt".into()));
        assert!(pending.contains(&"file.txt".into()));
        let status = engine.status.start(folder.id, 1);
        retry_fixture(&folder, &engine, &status).await;
        assert!(engine
            .state_db
            .find_by_local_path(folder.id, "file.txt")
            .unwrap()
            .is_some());
        assert!(server
            .requests
            .try_iter()
            .all(|request| !request.starts_with("PATCH ")));
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn startup_reconciles_retired_ids_but_rejects_live_duplicate_names() {
        for duplicate in [false, true] {
            let mut replies = vec![Reply::json(
                r#"{"files":[{"id":"new-id","name":"file.txt","mimeType":"text/plain","md5Checksum":"22af645d1859cb5ca6da0c484f1f37ea"}]}"#,
            )];
            if duplicate {
                replies.push(Reply::json(r#"{"id":"old-id","name":"file.txt","mimeType":"text/plain","parents":["root-id"]}"#));
            } else {
                replies.push(Reply::error(404));
                replies.push(Reply::json("new"));
            }
            let server = Server::start(replies);
            let (root, mut folder, mut engine) = local_fixture(&server, b"old").await;
            folder.drive_folder_id = Some("root-id".into());
            engine
                .state_db
                .complete_local_change(folder.id, "file.txt")
                .unwrap();
            engine.state_db = Arc::new(StateDb::open(&root.join("state.sqlite")).unwrap());
            let mut synced = 0;
            let result = initial_pull(
                &folder,
                &engine.drive,
                &engine.state_db,
                "root-id",
                "",
                (nix::unistd::Uid::current(), nix::unistd::Gid::current()),
                &mut synced,
            )
            .await;
            if duplicate {
                assert!(
                    matches!(result, Err(SyncError::Api(message)) if message.contains("multiple Drive"))
                );
                assert_eq!(
                    tokio::fs::read(folder.local_path.join("file.txt"))
                        .await
                        .unwrap(),
                    b"old"
                );
            } else {
                result.unwrap();
                assert_eq!(synced, 1);
                assert_eq!(
                    tokio::fs::read(folder.local_path.join("file.txt"))
                        .await
                        .unwrap(),
                    b"new"
                );
            }
            drop(engine);
            tokio::fs::remove_dir_all(root).await.unwrap();
        }
    }

    #[tokio::test]
    async fn moved_out_mapping_releases_its_path_before_an_earlier_creation() {
        let server = Server::start(vec![
            Reply::json(
                r#"{"changes":[{"fileId":"new-id","removed":false,"file":{"id":"new-id","name":"file.txt","mimeType":"text/plain","parents":["root-id"],"md5Checksum":"22af645d1859cb5ca6da0c484f1f37ea"}},{"fileId":"old-id","removed":false,"file":{"id":"old-id","name":"file.txt","mimeType":"text/plain","parents":["outside"]}}],"newStartPageToken":"next"}"#,
            ),
            Reply::json(
                r#"{"id":"outside","name":"outside","mimeType":"application/vnd.google-apps.folder"}"#,
            ),
            Reply::json("new"),
        ]);
        let (root, mut folder, engine) = local_fixture(&server, b"old").await;
        folder.drive_folder_id = Some("root-id".into());
        engine
            .state_db
            .complete_local_change(folder.id, "file.txt")
            .unwrap();
        engine.state_db.set_page_token(folder.id, "start").unwrap();
        poll_fixture(&folder, &engine).await.unwrap();
        assert_eq!(
            tokio::fs::read(folder.local_path.join("file.txt"))
                .await
                .unwrap(),
            b"new"
        );
        assert_eq!(
            engine
                .state_db
                .get_page_token(folder.id)
                .unwrap()
                .as_deref(),
            Some("next")
        );
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn recursive_discovery_does_not_overwrite_untracked_children() {
        let server = Server::start(vec![
            Reply::json(
                r#"{"changes":[{"fileId":"dir","removed":false,"file":{"id":"dir","name":"dir","mimeType":"application/vnd.google-apps.folder","parents":["root-id"]}}],"newStartPageToken":"next"}"#,
            ),
            Reply::json(
                r#"{"files":[{"id":"remote-child","name":"child.txt","mimeType":"text/plain","md5Checksum":"remote"}]}"#,
            ),
            Reply::json("remote"),
        ]);
        let (root, mut folder, engine) = local_fixture(&server, b"old").await;
        folder.drive_folder_id = Some("root-id".into());
        engine
            .state_db
            .complete_local_change(folder.id, "file.txt")
            .unwrap();
        engine.state_db.set_page_token(folder.id, "start").unwrap();
        tokio::fs::create_dir(folder.local_path.join("dir"))
            .await
            .unwrap();
        tokio::fs::write(folder.local_path.join("dir/child.txt"), b"local")
            .await
            .unwrap();
        assert!(poll_fixture(&folder, &engine).await.is_err());
        assert_eq!(
            tokio::fs::read(folder.local_path.join("dir/child.txt"))
                .await
                .unwrap(),
            b"local"
        );
        assert_eq!(
            engine
                .state_db
                .get_page_token(folder.id)
                .unwrap()
                .as_deref(),
            Some("start")
        );
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    async fn poll_fixture(folder: &SyncFolder, engine: &SyncEngine) -> Result<(), SyncError> {
        let status = engine.status.start(folder.id, 100);
        poll_remote_changes(
            folder,
            &engine.drive,
            &engine.state_db,
            nix::unistd::Uid::current(),
            nix::unistd::Gid::current(),
            &engine.events,
            &status,
        )
        .await
    }

    #[tokio::test]
    async fn discovers_nested_remote_files_and_replays_a_failed_poll_after_restart() {
        let changes = r#"{"changes":[{"fileId":"new-id","removed":false,"file":{"id":"new-id","name":"new.txt","mimeType":"text/plain","parents":["dir"],"md5Checksum":"22af645d1859cb5ca6da0c484f1f37ea"}}],"newStartPageToken":"next"}"#;
        let parent = r#"{"id":"dir","name":"nested","mimeType":"application/vnd.google-apps.folder","parents":["root-id"]}"#;
        let server = Server::start(vec![
            Reply::json(changes),
            Reply::json(parent),
            Reply::error(503),
            Reply::json(changes),
            Reply::json(parent),
            Reply::json("new"),
        ]);
        let (root, mut folder, mut engine) = local_fixture(&server, b"old").await;
        folder.drive_folder_id = Some("root-id".into());
        engine
            .state_db
            .complete_local_change(folder.id, "file.txt")
            .unwrap();
        engine
            .state_db
            .set_page_token(folder.id, "offline")
            .unwrap();
        assert!(poll_fixture(&folder, &engine).await.is_err());
        assert_eq!(
            engine
                .state_db
                .get_page_token(folder.id)
                .unwrap()
                .as_deref(),
            Some("offline")
        );
        engine.state_db = Arc::new(StateDb::open(&root.join("state.sqlite")).unwrap());
        poll_fixture(&folder, &engine).await.unwrap();
        assert_eq!(
            tokio::fs::read(folder.local_path.join("nested/new.txt"))
                .await
                .unwrap(),
            b"new"
        );
        assert_eq!(
            engine
                .state_db
                .get_page_token(folder.id)
                .unwrap()
                .as_deref(),
            Some("next")
        );
        let requests: Vec<_> = server.requests.try_iter().collect();
        assert!(requests[3].contains("pageToken=offline"));
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn remote_rename_relocates_the_file_without_downloading_unchanged_content() {
        let server = Server::start(vec![Reply::json(
            r#"{"changes":[{"fileId":"old-id","removed":false,"file":{"id":"old-id","name":"renamed.txt","mimeType":"text/plain","parents":["root-id"],"md5Checksum":"149603e6c03516362a8da23f624db945"}}],"newStartPageToken":"next"}"#,
        )]);
        let (root, mut folder, engine) = local_fixture(&server, b"old").await;
        folder.drive_folder_id = Some("root-id".into());
        engine
            .state_db
            .complete_local_change(folder.id, "file.txt")
            .unwrap();
        engine
            .state_db
            .set_page_token(folder.id, "offline")
            .unwrap();
        poll_fixture(&folder, &engine).await.unwrap();
        assert!(!folder.local_path.join("file.txt").exists());
        assert_eq!(
            tokio::fs::read(folder.local_path.join("renamed.txt"))
                .await
                .unwrap(),
            b"old"
        );
        assert!(engine
            .state_db
            .find_by_local_path(folder.id, "file.txt")
            .unwrap()
            .is_none());
        assert_eq!(server.requests.try_iter().count(), 1);
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn remote_directory_move_relocates_descendants_and_ignores_outside_creations() {
        let server = Server::start(vec![
            Reply::json(
                r#"{"changes":[{"fileId":"dir","removed":false,"file":{"id":"dir","name":"renamed","mimeType":"application/vnd.google-apps.folder","parents":["root-id"]}},{"fileId":"outside","removed":false,"file":{"id":"outside","name":"secret.txt","mimeType":"text/plain","parents":["unrelated"]}}],"newStartPageToken":"next"}"#,
            ),
            Reply::json(
                r#"{"id":"unrelated","name":"unrelated","mimeType":"application/vnd.google-apps.folder"}"#,
            ),
            Reply::json(r#"{"files":[]}"#),
        ]);
        let (root, mut folder, engine) = local_fixture(&server, b"old").await;
        folder.drive_folder_id = Some("root-id".into());
        engine
            .state_db
            .complete_local_change(folder.id, "file.txt")
            .unwrap();
        tokio::fs::create_dir(folder.local_path.join("dir"))
            .await
            .unwrap();
        tokio::fs::rename(
            folder.local_path.join("file.txt"),
            folder.local_path.join("dir/file.txt"),
        )
        .await
        .unwrap();
        engine
            .state_db
            .relocate_file_state(folder.id, "file.txt", "dir/file.txt")
            .unwrap();
        let dir = DriveFile {
            id: "dir".into(),
            name: "dir".into(),
            mime_type: FOLDER_MIME_TYPE.into(),
            parents: vec![],
            modified_time: None,
            trashed: false,
            md5_checksum: None,
            size: None,
        };
        record_state(folder.id, &engine.state_db, "dir", &dir, true).unwrap();
        engine
            .state_db
            .set_page_token(folder.id, "offline")
            .unwrap();
        poll_fixture(&folder, &engine).await.unwrap();
        assert_eq!(
            tokio::fs::read(folder.local_path.join("renamed/file.txt"))
                .await
                .unwrap(),
            b"old"
        );
        assert!(engine
            .state_db
            .find_by_local_path(folder.id, "dir/file.txt")
            .unwrap()
            .is_none());
        assert!(engine
            .state_db
            .find_by_local_path(folder.id, "renamed/file.txt")
            .unwrap()
            .is_some());
        assert!(!folder.local_path.join("secret.txt").exists());
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn same_name_remote_replacement_is_applied_even_when_creation_precedes_trash() {
        let server = Server::start(vec![
            Reply::json(
                r#"{"changes":[{"fileId":"new-id","removed":false,"file":{"id":"new-id","name":"file.txt","mimeType":"text/plain","parents":["root-id"],"md5Checksum":"22af645d1859cb5ca6da0c484f1f37ea"}},{"fileId":"old-id","removed":true}],"newStartPageToken":"next"}"#,
            ),
            Reply::json("new"),
        ]);
        let (root, mut folder, engine) = local_fixture(&server, b"old").await;
        folder.drive_folder_id = Some("root-id".into());
        engine
            .state_db
            .complete_local_change(folder.id, "file.txt")
            .unwrap();
        engine
            .state_db
            .set_page_token(folder.id, "offline")
            .unwrap();
        poll_fixture(&folder, &engine).await.unwrap();
        assert_eq!(
            tokio::fs::read(folder.local_path.join("file.txt"))
                .await
                .unwrap(),
            b"new"
        );
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
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn moved_out_remote_folder_removes_its_local_subtree() {
        let server = Server::start(vec![
            Reply::json(
                r#"{"changes":[{"fileId":"dir","removed":false,"file":{"id":"dir","name":"dir","mimeType":"application/vnd.google-apps.folder","parents":["elsewhere"]}}],"newStartPageToken":"next"}"#,
            ),
            Reply::json(
                r#"{"id":"elsewhere","name":"outside","mimeType":"application/vnd.google-apps.folder"}"#,
            ),
        ]);
        let (root, mut folder, engine) = local_fixture(&server, b"old").await;
        folder.drive_folder_id = Some("root-id".into());
        engine
            .state_db
            .complete_local_change(folder.id, "file.txt")
            .unwrap();
        tokio::fs::create_dir(folder.local_path.join("dir"))
            .await
            .unwrap();
        tokio::fs::rename(
            folder.local_path.join("file.txt"),
            folder.local_path.join("dir/file.txt"),
        )
        .await
        .unwrap();
        engine
            .state_db
            .relocate_file_state(folder.id, "file.txt", "dir/file.txt")
            .unwrap();
        let dir = DriveFile {
            id: "dir".into(),
            name: "dir".into(),
            mime_type: FOLDER_MIME_TYPE.into(),
            parents: vec![],
            modified_time: None,
            trashed: false,
            md5_checksum: None,
            size: None,
        };
        record_state(folder.id, &engine.state_db, "dir", &dir, true).unwrap();
        engine
            .state_db
            .set_page_token(folder.id, "offline")
            .unwrap();
        poll_fixture(&folder, &engine).await.unwrap();
        assert!(!folder.local_path.join("dir").exists());
        assert!(engine
            .state_db
            .find_by_local_path(folder.id, "dir/file.txt")
            .unwrap()
            .is_none());
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

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
            Reply::json(UPLOADED_NEW),
            Reply::json(REMOTE_OLD),
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
        assert!(requests[2].starts_with("GET /files/new-id?"));
        assert!(requests[3].starts_with("GET /files/old-id?"));
        assert!(requests[4].starts_with("PATCH /files/old-id "));
        assert!(requests[4].contains(r#""trashed":true"#));
        drop(status);
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn failed_trash_resumes_uploaded_replacement_after_restart() {
        let server = Server::start(vec![
            Reply::json(r#"{"ids":["new-id"]}"#),
            Reply::json(UPLOADED_NEW),
            Reply::json(UPLOADED_NEW),
            Reply::json(REMOTE_OLD),
            Reply::error(503),
            Reply::json(UPLOADED_NEW),
            Reply::json(REMOTE_OLD),
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
        assert!(requests[4].starts_with("PATCH /files/old-id "));
        engine.state_db = Arc::new(StateDb::open(&root.join("state.sqlite")).unwrap());
        retry_fixture(&folder, &engine, &status).await;
        let requests: Vec<_> = server.requests.try_iter().collect();
        assert_eq!(requests.len(), 3, "retry must not create another copy");
        assert!(requests[0].starts_with("GET /files/new-id?"));
        assert!(requests[2].starts_with("PATCH /files/old-id "));
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
            Reply::json(UPLOADED_NEW),
            Reply::json(REMOTE_OLD),
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
        assert!(requests[3].starts_with("GET /files/new-id?"));
        assert!(requests[5].starts_with("PATCH /files/old-id "));
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
            Reply::json(
                r#"{"id":"parent-id","name":"dir","mimeType":"application/vnd.google-apps.folder"}"#,
            ),
            Reply::json(r#"{"ids":["new-id"]}"#),
            Reply::json(UPLOADED_NEW),
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
        assert!(requests[4].contains(r#""parents":["parent-id"]"#));
        drop(status);
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn backslash_filename_is_replayed_verbatim_not_as_a_directory() {
        const UPLOADED: &str = r#"{"id":"new-id","name":"report\\2026.txt","mimeType":"text/plain","md5Checksum":"22af645d1859cb5ca6da0c484f1f37ea"}"#;
        let server = Server::start(vec![
            Reply::json(r#"{"ids":["new-id"]}"#),
            Reply::json(UPLOADED),
            Reply::json(UPLOADED),
            Reply::json(REMOTE_OLD),
            Reply::json("{}"),
        ]);
        let (root, folder, engine) = local_fixture(&server, b"new").await;
        let name = r"report\2026.txt";
        tokio::fs::rename(
            folder.local_path.join("file.txt"),
            folder.local_path.join(name),
        )
        .await
        .unwrap();
        let mut entry = engine
            .state_db
            .find_by_local_path(folder.id, "file.txt")
            .unwrap()
            .unwrap();
        engine
            .state_db
            .remove_file_state(folder.id, "file.txt")
            .unwrap();
        engine
            .state_db
            .complete_local_change(folder.id, "file.txt")
            .unwrap();
        entry.local_rel_path = path_key(Path::new(name)).unwrap();
        engine.state_db.upsert_file_state(&entry).unwrap();
        engine
            .state_db
            .queue_local_change(folder.id, &entry.local_rel_path)
            .unwrap();
        let status = engine.status.start(folder.id, 1);
        retry_fixture(&folder, &engine, &status).await;
        assert!(engine
            .state_db
            .pending_local_paths(folder.id)
            .unwrap()
            .is_empty());
        assert_eq!(
            engine
                .state_db
                .find_by_local_path(folder.id, name)
                .unwrap()
                .unwrap()
                .drive_file_id
                .as_deref(),
            Some("new-id")
        );
        assert!(!folder.local_path.join("report").exists());
        let requests: Vec<_> = server.requests.try_iter().collect();
        assert!(requests[1].contains(r#""name":"report\\2026.txt""#));
        assert!(requests[4].starts_with("PATCH /files/old-id "));
        drop(status);
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn missing_checksum_preserves_offline_edit_during_initial_pull() {
        let server = Server::start(vec![Reply::json(
            r#"{"files":[{"id":"old-id","name":"file.txt","mimeType":"text/plain","md5Checksum":"22af645d1859cb5ca6da0c484f1f37ea"}]}"#,
        )]);
        let (root, folder, engine) = local_fixture(&server, b"offline edit").await;
        engine
            .state_db
            .complete_local_change(folder.id, "file.txt")
            .unwrap();
        let mut entry = engine
            .state_db
            .find_by_local_path(folder.id, "file.txt")
            .unwrap()
            .unwrap();
        entry.md5_checksum = None;
        engine.state_db.upsert_file_state(&entry).unwrap();
        let mut synced = 0;
        initial_pull(
            &folder,
            &engine.drive,
            &engine.state_db,
            "root",
            "",
            (nix::unistd::Uid::current(), nix::unistd::Gid::current()),
            &mut synced,
        )
        .await
        .unwrap();
        assert_eq!(synced, 0);
        assert_eq!(
            tokio::fs::read(folder.local_path.join("file.txt"))
                .await
                .unwrap(),
            b"offline edit"
        );
        assert_eq!(
            engine.state_db.pending_local_paths(folder.id).unwrap(),
            vec!["file.txt"]
        );
        assert_eq!(server.requests.try_iter().count(), 1);
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn missing_or_trashed_replacement_never_retires_the_surviving_old_copy() {
        for gone in [
            Reply::error(404),
            Reply::json(
                r#"{"id":"new-id","name":"file.txt","mimeType":"text/plain","trashed":true}"#,
            ),
        ] {
            let server = Server::start(vec![gone]);
            let (root, folder, engine) = local_fixture(&server, b"new").await;
            engine
                .state_db
                .set_pending_upload(
                    folder.id,
                    "file.txt",
                    Some(&PendingUpload {
                        file_id: "new-id".into(),
                        previous_drive_id: Some("old-id".into()),
                        is_folder: false,
                        uploaded: Some(serde_json::from_str(UPLOADED_NEW).unwrap()),
                    }),
                )
                .unwrap();
            let status = engine.status.start(folder.id, 1);
            retry_fixture(&folder, &engine, &status).await;
            let requests: Vec<_> = server.requests.try_iter().collect();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].starts_with("GET /files/new-id?"));
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
            assert_eq!(
                engine.state_db.pending_local_paths(folder.id).unwrap(),
                vec!["file.txt"]
            );
            assert!(engine
                .state_db
                .pending_upload(folder.id, "file.txt")
                .unwrap()
                .is_none());
            assert!(!engine.sync_status().folders[0].errors.is_empty());
            drop(status);
            drop(engine);
            tokio::fs::remove_dir_all(root).await.unwrap();
        }
    }

    #[tokio::test]
    async fn directory_deletion_protects_offline_and_in_flight_child_edits() {
        for edit_in_flight in [false, true] {
            let mut removal = Reply::json(
                r#"{"changes":[{"fileId":"dir-id","removed":true}],"newStartPageToken":"removed"}"#,
            );
            removal.header_delay = Duration::from_millis(200);
            let server = Server::start(vec![removal]);
            let (root, folder, engine) = local_fixture(&server, b"old").await;
            let directory = folder.local_path.join("dir");
            tokio::fs::create_dir(&directory).await.unwrap();
            tokio::fs::rename(
                folder.local_path.join("file.txt"),
                directory.join("file.txt"),
            )
            .await
            .unwrap();
            let mut child = engine
                .state_db
                .find_by_local_path(folder.id, "file.txt")
                .unwrap()
                .unwrap();
            engine
                .state_db
                .remove_file_state(folder.id, "file.txt")
                .unwrap();
            engine
                .state_db
                .complete_local_change(folder.id, "file.txt")
                .unwrap();
            child.local_rel_path = "dir/file.txt".into();
            engine.state_db.upsert_file_state(&child).unwrap();
            let mut parent = child.clone();
            parent.local_rel_path = "dir".into();
            parent.drive_file_id = Some("dir-id".into());
            parent.is_folder = true;
            parent.md5_checksum = None;
            engine.state_db.upsert_file_state(&parent).unwrap();
            let mut replaced_directory = parent.clone();
            replaced_directory.local_rel_path = "dir/former-directory".into();
            replaced_directory.drive_file_id = Some("former-dir-id".into());
            engine
                .state_db
                .upsert_file_state(&replaced_directory)
                .unwrap();
            tokio::fs::write(
                directory.join("former-directory"),
                b"replaced directory locally",
            )
            .await
            .unwrap();
            tokio::fs::write(directory.join("local-only.txt"), b"created offline")
                .await
                .unwrap();
            if !edit_in_flight {
                tokio::fs::write(directory.join("file.txt"), b"edited offline")
                    .await
                    .unwrap();
            }
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
                    if edit_in_flight {
                        tokio::time::timeout(Duration::from_secs(2), async {
                            while server.requests.try_recv().is_err() {
                                tokio::task::yield_now().await;
                            }
                        })
                        .await
                        .unwrap();
                        tokio::fs::write(directory.join("file.txt"), b"edited offline")
                            .await
                            .unwrap();
                    }
                }
            );
            result.unwrap();
            let pending = engine.state_db.pending_local_paths(folder.id).unwrap();
            let conflict = pending
                .iter()
                .find(|key| key.starts_with("dir.local-conflict-") && !key.contains('/'))
                .unwrap();
            let preserved = folder.local_path.join(conflict);
            assert!(!directory.exists());
            assert_eq!(
                tokio::fs::read(preserved.join("file.txt")).await.unwrap(),
                b"edited offline"
            );
            assert_eq!(
                tokio::fs::read(preserved.join("local-only.txt"))
                    .await
                    .unwrap(),
                b"created offline"
            );
            assert!(pending.contains(&format!("{conflict}/file.txt")));
            assert!(pending.contains(&format!("{conflict}/former-directory")));
            assert!(pending.contains(&format!("{conflict}/local-only.txt")));
            assert!(engine
                .state_db
                .find_by_drive_id(folder.id, "dir-id")
                .unwrap()
                .is_none());
            assert_eq!(
                engine
                    .state_db
                    .get_page_token(folder.id)
                    .unwrap()
                    .as_deref(),
                Some("removed")
            );
            drop(status);
            drop(engine);
            tokio::fs::remove_dir_all(root).await.unwrap();
        }
    }

    #[tokio::test]
    async fn watcher_persists_local_only_file_before_transfer_cancellation() {
        let mut stalled = Reply::json("old");
        stalled.header_delay = Duration::from_secs(1);
        let server = Server::start(vec![stalled]);
        let (root, folder, engine) = local_fixture(&server, b"old").await;
        engine
            .state_db
            .complete_local_change(folder.id, "file.txt")
            .unwrap();
        let (sender, receiver) = mpsc::unbounded_channel();
        let watcher = LocalWatcher::watch(
            &folder.local_path,
            folder.id,
            Arc::clone(&engine.state_db),
            sender,
        )
        .unwrap();
        let drive = Arc::clone(&engine.drive);
        let transfer = tokio::spawn(async move { drive.download_file("old-id").await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while server.requests.try_recv().is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let name = r"created\offline.txt";
        tokio::fs::write(folder.local_path.join(name), b"local only")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !engine
                .state_db
                .pending_local_paths(folder.id)
                .unwrap()
                .iter()
                .any(|path| path == name)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            !transfer.is_finished(),
            "queue intake must not wait for the transfer"
        );
        transfer.abort();
        assert!(transfer.await.unwrap_err().is_cancelled());
        drop(watcher);
        drop(receiver);
        let reopened = StateDb::open(&root.join("state.sqlite")).unwrap();
        assert_eq!(reopened.pending_local_paths(folder.id).unwrap(), vec![name]);
        drop(reopened);
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn deferred_remote_edit_replays_when_failed_local_upload_is_reverted() {
        const REMOTE_EDIT: &str = r#"{"changes":[{"fileId":"old-id","removed":false,"file":{"id":"old-id","name":"file.txt","mimeType":"text/plain","md5Checksum":"22af645d1859cb5ca6da0c484f1f37ea"}}],"newStartPageToken":"updated"}"#;
        let server = Server::start(vec![
            Reply::error(503),
            Reply::json(REMOTE_EDIT),
            Reply::json(REMOTE_EDIT),
            Reply::json("new"),
        ]);
        let (root, folder, engine) = local_fixture(&server, b"new").await;
        let status = engine.status.start(folder.id, 1);
        engine.state_db.set_page_token(folder.id, "start").unwrap();
        retry_fixture(&folder, &engine, &status).await;
        assert!(!engine.sync_status().folders[0].errors.is_empty());
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
        assert_eq!(
            engine
                .state_db
                .get_page_token(folder.id)
                .unwrap()
                .as_deref(),
            Some("start")
        );
        tokio::fs::write(folder.local_path.join("file.txt"), b"old")
            .await
            .unwrap();
        retry_fixture(&folder, &engine, &status).await;
        assert!(engine
            .state_db
            .pending_local_paths(folder.id)
            .unwrap()
            .is_empty());
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
        assert_eq!(
            tokio::fs::read(folder.local_path.join("file.txt"))
                .await
                .unwrap(),
            b"new"
        );
        assert_eq!(
            engine
                .state_db
                .get_page_token(folder.id)
                .unwrap()
                .as_deref(),
            Some("updated")
        );
        let requests: Vec<_> = server.requests.try_iter().collect();
        assert!(requests[2].contains("pageToken=start"));
        drop(status);
        drop(engine);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn worker_retries_persisted_local_upload_without_another_filesystem_event() {
        let server = Server::start(vec![
            Reply::json(r#"{"startPageToken":"start"}"#),
            Reply::json(
                r#"{"files":[{"id":"old-id","name":"file.txt","mimeType":"text/plain","md5Checksum":"149603e6c03516362a8da23f624db945"}]}"#,
            ),
            Reply::json(r#"{"ids":["new-id"]}"#),
            Reply::error(503),
            Reply::json(r#"{"changes":[],"newStartPageToken":"retry"}"#),
            Reply::error(404),
            Reply::json(UPLOADED_NEW),
            Reply::json(UPLOADED_NEW),
            Reply::json(REMOTE_OLD),
            Reply::json("{}"),
            Reply::json(r#"{"changes":[],"newStartPageToken":"polled"}"#),
        ]);
        let (root, folder, engine) = local_fixture(&server, b"new").await;
        let mut events = engine.subscribe();
        let run = engine.status.start(folder.id, 1);
        let worker = tokio::spawn({
            let folder = folder.clone();
            let engine = engine.clone();
            async move {
                run_folder(
                    folder,
                    engine.state_db,
                    engine.drive,
                    engine.events,
                    Duration::from_millis(50),
                    &run,
                )
                .await
            }
        });
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
        worker.abort();
        let _ = worker.await;
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
