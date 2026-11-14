//! Watches a local directory tree for filesystem changes using the
//! `notify` crate (inotify on Linux, kqueue on BSD/macOS).

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::error::SyncError;
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
    _watcher: RecommendedWatcher,
}

impl LocalWatcher {
    /// Starts watching `root` recursively, sending [`LocalChange`] events to
    /// `sender` as they occur.
    pub fn watch(
        root: &Path,
        folder_id: Uuid,
        state_db: Arc<StateDb>,
        sender: mpsc::UnboundedSender<Result<LocalChange, SyncError>>,
    ) -> Result<Self, SyncError> {
        let watched_root = root.to_path_buf();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<Event>| match res {
                Ok(event) => {
                    let Some(kind) = classify(&event.kind) else {
                        return;
                    };
                    for path in event.paths.clone() {
                        let Ok(relative) = path.strip_prefix(&watched_root) else {
                            continue;
                        };
                        if relative.as_os_str().is_empty() || is_download_staging(relative) {
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

        watcher.watch(root, RecursiveMode::Recursive)?;

        Ok(Self { _watcher: watcher })
    }
}

pub(crate) fn path_key(path: &Path) -> Result<String, SyncError> {
    Ok(std::str::from_utf8(path.as_os_str().as_bytes())?.to_owned())
}

pub(crate) fn is_download_staging(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| name.starts_with(".gdrive-download-"))
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
