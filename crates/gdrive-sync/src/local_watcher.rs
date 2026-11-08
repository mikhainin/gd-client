//! Watches a local directory tree for filesystem changes using the
//! `notify` crate (inotify on Linux, kqueue on BSD/macOS).

use std::path::{Path, PathBuf};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::error::SyncError;

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
        sender: mpsc::UnboundedSender<LocalChange>,
    ) -> Result<Self, SyncError> {
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<Event>| match res {
                Ok(event) => {
                    let Some(kind) = classify(&event.kind) else {
                        return;
                    };
                    for path in event.paths.clone() {
                        let _ = sender.send(LocalChange { path, kind });
                    }
                }
                Err(err) => {
                    tracing::warn!("local filesystem watch error: {err}");
                }
            })?;

        watcher.watch(root, RecursiveMode::Recursive)?;

        Ok(Self { _watcher: watcher })
    }
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
