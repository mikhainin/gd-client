//! Watches a local directory tree for filesystem changes using the
//! `notify` crate (inotify on Linux, kqueue on BSD/macOS).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::error::SyncError;
use crate::local_fs::{blocking_fs, Directory, LocalFs};
use crate::state_db::StateDb;
use uuid::Uuid;

/// A simplified filesystem change notification forwarded to the sync engine.
#[derive(Debug, Clone)]
pub struct LocalChange {
    pub path: PathBuf,
    pub kind: LocalChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalChangeKind {
    Created,
    Modified,
    Removed,
    /// Renames and other events we don't need to distinguish further; the
    /// engine will re-scan the affected paths.
    Other,
}

/// Owns a `notify` watcher for one local sync root and forwards debounced
/// change events over an async channel.
pub struct LocalWatcher {
    // Kept alive for as long as we want to keep receiving events; dropping
    // it stops the underlying OS watch.
    watcher: RecommendedWatcher,
    root: PathBuf,
    watched: HashMap<PathBuf, WatchedDirectory>,
    paths: Arc<Mutex<HashMap<PathBuf, PathBuf>>>,
}

struct WatchedDirectory {
    directory: Directory,
    identity: (u64, u64),
}

impl WatchedDirectory {
    fn watch_path(&self) -> PathBuf {
        self.directory.path(OsStr::new("."))
    }
}

impl LocalWatcher {
    /// Watches live directories without registering internal recovery/staging trees.
    pub fn watch(
        root: &Path,
        folder_id: Uuid,
        state_db: Arc<StateDb>,
        sender: mpsc::UnboundedSender<Result<LocalChange, SyncError>>,
    ) -> Result<Self, SyncError> {
        let watched_root = root.to_path_buf();
        let paths = Arc::new(Mutex::new(HashMap::<PathBuf, PathBuf>::new()));
        let event_paths = Arc::clone(&paths);
        let watcher = notify::recommended_watcher(move |res: notify::Result<Event>| match res {
            Ok(event) => {
                let Some(kind) = classify(&event.kind) else {
                    return;
                };
                for event_path in &event.paths {
                    let path = {
                        let paths = event_paths.lock().unwrap();
                        paths.iter().find_map(|(registered, logical)| {
                            event_path
                                .strip_prefix(registered)
                                .ok()
                                .map(|relative| logical.join(relative))
                        })
                    };
                    let Some(path) = path else { continue };
                    let Ok(relative) = path.strip_prefix(&watched_root) else {
                        continue;
                    };
                    if relative.as_os_str().is_empty() || is_internal_path(relative) {
                        continue;
                    }
                    let result = path_key(relative)
                        .and_then(|key| state_db.queue_local_change(folder_id, &key))
                        .map(|()| LocalChange { path, kind });
                    if let Err(error) = &result {
                        tracing::warn!("could not persist filesystem change: {error}");
                    }
                    let _ = sender.send(result);
                }
            }
            Err(err) => {
                tracing::warn!("local filesystem watch error: {err}");
                let _ = sender.send(Err(err.into()));
            }
        })?;

        let mut watcher = Self {
            watcher,
            root: root.to_path_buf(),
            watched: HashMap::new(),
            paths,
        };
        watcher.refresh()?;
        Ok(watcher)
    }

    /// Reconcile live-directory registrations after topology or inode changes.
    pub fn refresh(&mut self) -> Result<(), SyncError> {
        self.refresh_with(|_| Ok(()))
    }

    fn refresh_with(
        &mut self,
        mut before_open: impl FnMut(&Path) -> std::io::Result<()>,
    ) -> Result<(), SyncError> {
        blocking_fs(|| {
            let mut desired = HashMap::new();
            let mut directories = vec![self.root.clone()];
            let inspection = (|| {
                while let Some(directory) = directories.pop() {
                    before_open(&directory)?;
                    let mut items = match LocalFs(&self.root).read_dir(&directory) {
                        Ok(items) => items,
                        Err(error)
                            if directory != self.root
                                && matches!(
                                    error.kind(),
                                    std::io::ErrorKind::NotFound
                                        | std::io::ErrorKind::NotADirectory
                                ) =>
                        {
                            continue
                        }
                        Err(error) => return Err(error.into()),
                    };
                    for item in items.by_ref() {
                        let (name, file_type) = item?;
                        let path = directory.join(name);
                        if file_type.is_dir() && !is_internal_path(path.strip_prefix(&self.root)?) {
                            directories.push(path);
                        }
                    }
                    let pin = items.into_directory();
                    let metadata = pin.metadata()?;
                    desired.insert(
                        directory,
                        WatchedDirectory {
                            directory: pin,
                            identity: (metadata.dev(), metadata.ino()),
                        },
                    );
                }
                Ok::<_, SyncError>(())
            })();
            for path in self
                .watched
                .iter()
                .filter(|(path, watched)| {
                    desired
                        .get(*path)
                        .is_none_or(|current| current.identity != watched.identity)
                })
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>()
            {
                let registered = self.watched[&path].watch_path();
                match self.watcher.unwatch(&registered) {
                    Ok(()) => {}
                    Err(error)
                        if matches!(
                            error.kind,
                            notify::ErrorKind::WatchNotFound | notify::ErrorKind::PathNotFound
                        ) => {}
                    Err(error) => return Err(error.into()),
                }
                self.paths.lock().unwrap().remove(&registered);
                self.watched.remove(&path);
            }
            inspection?;
            for (path, watched) in desired {
                if self.watched.contains_key(&path) {
                    continue;
                }
                let registered = watched.watch_path();
                self.paths
                    .lock()
                    .unwrap()
                    .insert(registered.clone(), path.clone());
                match self.watcher.watch(&registered, RecursiveMode::NonRecursive) {
                    Ok(()) => {
                        self.watched.insert(path, watched);
                    }
                    Err(error) => {
                        self.paths.lock().unwrap().remove(&registered);
                        if !matches!(error.kind, notify::ErrorKind::PathNotFound) {
                            return Err(error.into());
                        }
                    }
                }
            }
            Ok(())
        })
    }
}

pub(crate) fn path_key(path: &Path) -> Result<String, SyncError> {
    Ok(std::str::from_utf8(path.as_os_str().as_bytes())?.to_owned())
}

pub(crate) fn is_internal_path(path: &Path) -> bool {
    path.components().any(|component| {
        component.as_os_str().to_str().is_some_and(|name| {
            name.starts_with(".gdrive-download-")
                || name.starts_with(crate::recovery::INITIALIZING_PREFIX)
                || name == crate::recovery::DIRECTORY
        })
    })
}

fn classify(kind: &EventKind) -> Option<LocalChangeKind> {
    match kind {
        EventKind::Create(_) => Some(LocalChangeKind::Created),
        EventKind::Modify(notify::event::ModifyKind::Metadata(_)) | EventKind::Access(_) => None,
        EventKind::Modify(_) => Some(LocalChangeKind::Modified),
        EventKind::Remove(_) => Some(LocalChangeKind::Removed),
        _ => Some(LocalChangeKind::Other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, DataChange, MetadataKind, ModifyKind};
    use std::collections::HashSet;

    #[cfg(target_os = "linux")]
    #[test]
    fn queued_symlink_swaps_cannot_watch_another_tree_or_leave_history_watched() {
        let fixture = std::env::temp_dir().join(format!("gdrive-watches-{}", Uuid::new_v4()));
        let root = fixture.join("local");
        let other = fixture.join("other/deep");
        std::fs::create_dir_all(root.join("live/deep")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir(root.join(crate::recovery::DIRECTORY)).unwrap();
        let state = Arc::new(StateDb::open(&fixture.join("state.sqlite")).unwrap());
        let (sender, _receiver) = mpsc::unbounded_channel();
        let mut watcher = LocalWatcher::watch(&root, Uuid::new_v4(), state, sender).unwrap();
        let archived = root.join(crate::recovery::DIRECTORY).join("moved");
        assert!(watcher
            .refresh_with(|path| {
                if path == root.join("live") {
                    std::fs::rename(path, &archived)?;
                    std::os::unix::fs::symlink(other.parent().unwrap(), path)?;
                }
                Ok(())
            })
            .is_err());
        let watches = kernel_watch_inodes();
        for path in [
            &other,
            other.parent().unwrap(),
            &archived,
            &archived.join("deep"),
        ] {
            assert!(!watches.contains(&std::fs::metadata(path).unwrap().ino()));
        }
        drop(watcher);
        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn stalled_refresh_does_not_occupy_the_only_async_worker() {
        let fixture = std::env::temp_dir().join(format!("gdrive-watches-{}", Uuid::new_v4()));
        let root = fixture.join("local");
        std::fs::create_dir_all(&root).unwrap();
        let state = Arc::new(StateDb::open(&fixture.join("state.sqlite")).unwrap());
        let (sender, _receiver) = mpsc::unbounded_channel();
        let mut watcher = LocalWatcher::watch(&root, Uuid::new_v4(), state, sender).unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (resume, wait) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            let mut started = Some(started);
            watcher.refresh_with(|_| {
                if let Some(started) = started.take() {
                    started.send(()).unwrap();
                    wait.recv_timeout(std::time::Duration::from_secs(2))
                        .map_err(std::io::Error::other)?;
                }
                Ok(())
            })
        });
        ready.await.unwrap();
        assert!(!task.is_finished());
        resume.send(()).unwrap();
        task.await.unwrap().unwrap();
        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn recovery_entries_and_download_staging_never_enter_the_upload_queue() {
        for key in [
            ".gdrive-recovery/id/file",
            "folder/.gdrive-recovery/id/child",
            "folder/.gdrive-download-id",
            "folder/.gdrive-recovery-init-id/owner",
        ] {
            assert!(is_internal_path(Path::new(key)));
        }
        assert!(!is_internal_path(Path::new("recovery/file")));
    }

    #[cfg(target_os = "linux")]
    fn kernel_watch_inodes() -> HashSet<u64> {
        let mut inodes = HashSet::new();
        for entry in std::fs::read_dir("/proc/self/fdinfo").unwrap() {
            let contents = match std::fs::read_to_string(entry.unwrap().path()) {
                Ok(contents) => contents,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => panic!("cannot inspect owned inotify watches: {error}"),
            };
            for line in contents.lines().filter(|line| line.starts_with("inotify ")) {
                let inode = line
                    .split_whitespace()
                    .find_map(|field| field.strip_prefix("ino:"))
                    .unwrap();
                inodes.insert(u64::from_str_radix(inode, 16).unwrap());
            }
        }
        inodes
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn kernel_watches_exclude_history_and_refresh_replaced_live_directories() {
        use std::os::unix::fs::MetadataExt;
        let fixture = std::env::temp_dir().join(format!("gdrive-watches-{}", Uuid::new_v4()));
        let root = fixture.join("local");
        let live = root.join("live/deep");
        std::fs::create_dir_all(&live).unwrap();
        let saved = root.join("saved");
        std::fs::write(&saved, b"history").unwrap();
        crate::recovery::Recovery::new(&root, uuid::Uuid::nil())
            .archive(&saved)
            .unwrap()
            .unwrap();
        let recovery = root.join(crate::recovery::DIRECTORY);
        let history = recovery.join("older/deep");
        std::fs::create_dir_all(&history).unwrap();
        let state = Arc::new(StateDb::open(&fixture.join("state.sqlite")).unwrap());
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut watcher = LocalWatcher::watch(&root, Uuid::new_v4(), state, sender).unwrap();
        let watches = kernel_watch_inodes();
        assert!(watches.contains(&std::fs::metadata(&root).unwrap().ino()));
        for path in [&recovery, &history] {
            assert!(!watches.contains(&std::fs::metadata(path).unwrap().ino()));
        }
        std::fs::rename(root.join("live"), recovery.join("moved")).unwrap();
        std::fs::create_dir_all(&live).unwrap();
        watcher.refresh().unwrap();
        let watches = kernel_watch_inodes();
        assert!(watches.contains(&std::fs::metadata(&live).unwrap().ino()));
        assert!(!watches.contains(&std::fs::metadata(recovery.join("moved")).unwrap().ino()));
        assert!(!watches.contains(
            &std::fs::metadata(recovery.join("moved/deep"))
                .unwrap()
                .ino()
        ));
        let new = root.join("new/deep");
        std::fs::create_dir_all(&new).unwrap();
        watcher.refresh().unwrap();
        let file = new.join("file");
        std::fs::write(&file, b"changed").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if receiver.recv().await.unwrap().unwrap().path == file {
                    break;
                }
            }
        })
        .await
        .expect("new nested live directories must still emit events");
        drop(watcher);
        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn non_utf8_paths_fail_explicitly_instead_of_aliasing_another_file() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(OsString::from_vec(vec![b'a', 0xff]));
        assert!(matches!(
            path_key(&path),
            Err(SyncError::LocalPathEncoding(_))
        ));
    }

    #[test]
    fn access_and_ownership_events_are_ignored_but_content_changes_are_forwarded() {
        assert!(classify(&EventKind::Access(AccessKind::Read)).is_none());
        assert!(classify(&EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Ownership
        )))
        .is_none());
        assert!(matches!(
            classify(&EventKind::Modify(ModifyKind::Data(DataChange::Content))),
            Some(LocalChangeKind::Modified)
        ));
    }
}
