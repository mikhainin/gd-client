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
    pub fn watch(root: &Path, sender: mpsc::UnboundedSender<LocalChange>) -> Result<Self, SyncError> {
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| match res {
            Ok(event) => {
                for path in event.paths.clone() {
                    let kind = classify(&event.kind);
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

fn classify(kind: &EventKind) -> LocalChangeKind {
    match kind {
        EventKind::Create(_) => LocalChangeKind::Created,
        EventKind::Modify(_) => LocalChangeKind::Modified,
        EventKind::Remove(_) => LocalChangeKind::Removed,
        _ => LocalChangeKind::Other,
    }
}
