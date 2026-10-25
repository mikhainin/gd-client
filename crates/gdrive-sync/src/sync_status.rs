use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use gdrive_common::dbus_api::{FolderSyncStatus, SyncStatus};
use tokio::sync::watch;
use uuid::Uuid;

#[derive(Default)]
struct FolderStatus {
    generation: u64,
    syncing: bool,
    errors: BTreeMap<String, String>,
}

#[derive(Default)]
struct State {
    revision: u64,
    folders: BTreeMap<Uuid, FolderStatus>,
}

/// Snapshots survive absent/lagging subscribers; notifications are only hints.
#[derive(Clone)]
pub(crate) struct StatusTracker {
    state: Arc<Mutex<State>>,
    changes: watch::Sender<u64>,
}

impl Default for StatusTracker {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            changes: watch::channel(0).0,
        }
    }
}

impl StatusTracker {
    fn update(&self, update: impl FnOnce(&mut State) -> bool) {
        let mut state = self.state.lock().unwrap();
        if update(&mut state) {
            state.revision += 1;
            self.changes.send_replace(state.revision);
        }
    }

    pub(crate) fn start(&self, id: Uuid, generation: u64) -> FolderRun {
        self.update(|state| {
            state.folders.insert(
                id,
                FolderStatus {
                    generation,
                    syncing: true,
                    errors: BTreeMap::new(),
                },
            );
            true
        });
        FolderRun {
            tracker: self.clone(),
            id,
            generation,
        }
    }

    pub(crate) fn stop(&self, id: Uuid) {
        self.update(|state| state.folders.remove(&id).is_some());
    }

    pub(crate) fn snapshot(&self) -> SyncStatus {
        let state = self.state.lock().unwrap();
        SyncStatus {
            revision: state.revision,
            folders: state
                .folders
                .iter()
                .map(|(id, folder)| FolderSyncStatus {
                    folder_id: id.to_string(),
                    syncing: folder.syncing,
                    errors: folder
                        .errors
                        .iter()
                        .map(|(work, error)| format!("{work}: {error}"))
                        .collect(),
                })
                .collect(),
        }
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
}

pub(crate) struct FolderRun {
    tracker: StatusTracker,
    id: Uuid,
    generation: u64,
}

impl FolderRun {
    fn update(&self, update: impl FnOnce(&mut FolderStatus)) {
        self.tracker.update(|state| {
            let Some(folder) = state
                .folders
                .get_mut(&self.id)
                .filter(|folder| folder.generation == self.generation)
            else {
                return false;
            };
            let before = (folder.syncing, folder.errors.clone());
            update(folder);
            before != (folder.syncing, folder.errors.clone())
        });
    }

    pub(crate) fn active(&self) {
        self.update(|folder| folder.syncing = true);
    }

    pub(crate) fn finish(&self, work: &str, result: Result<(), String>) {
        self.update(|folder| {
            folder.syncing = false;
            match result {
                Ok(()) => {
                    folder.errors.remove(work);
                }
                Err(error) => {
                    folder.errors.insert(work.to_owned(), error);
                }
            }
        });
    }
}

impl Drop for FolderRun {
    fn drop(&mut self) {
        // Aborted and superseded workers cannot leave activity stuck on.
        self.update(|folder| {
            folder.syncing = false;
            if std::thread::panicking() {
                folder.errors.insert(
                    "worker".into(),
                    "sync task panicked; restart the folder".into(),
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unexpected_worker_unwind_is_an_error_not_idle() {
        let tracker = StatusTracker::default();
        let worker_tracker = tracker.clone();
        assert!(std::thread::spawn(move || {
            let _run = worker_tracker.start(Uuid::new_v4(), 1);
            panic!("fixture worker failure");
        })
        .join()
        .is_err());
        let snapshot = tracker.snapshot();
        assert!(!snapshot.folders[0].syncing);
        assert!(snapshot.folders[0].errors[0].contains("panicked"));
    }

    #[test]
    fn errors_survive_unrelated_success_and_other_folders() {
        let tracker = StatusTracker::default();
        let id = Uuid::new_v4();
        let run = tracker.start(id, 1);
        run.finish("file.txt", Err("upload failed".into()));
        run.finish("remote check", Ok(()));
        let other = tracker.start(Uuid::new_v4(), 2);
        other.finish("initial sync", Ok(()));
        let snapshot = tracker.snapshot();
        assert_eq!(
            snapshot
                .folders
                .iter()
                .map(|f| f.errors.len())
                .sum::<usize>(),
            1
        );
        run.finish("file.txt", Ok(()));
        assert!(tracker
            .snapshot()
            .folders
            .iter()
            .all(|f| f.errors.is_empty()));
    }

    #[test]
    fn stop_restart_and_drop_reject_obsolete_workers() {
        let tracker = StatusTracker::default();
        let id = Uuid::new_v4();
        let old = tracker.start(id, 1);
        tracker.stop(id);
        assert!(tracker.snapshot().folders.is_empty());
        let current = tracker.start(id, 2);
        old.finish("initial sync", Err("late failure".into()));
        drop(old);
        assert!(tracker.snapshot().folders[0].syncing);
        assert!(tracker.snapshot().folders[0].errors.is_empty());
        drop(current);
        assert!(!tracker.snapshot().folders[0].syncing);
    }

    #[test]
    fn activity_aggregates_and_notifications_recover_without_a_subscriber() {
        let tracker = StatusTracker::default();
        let first = tracker.start(Uuid::new_v4(), 1);
        let second = tracker.start(Uuid::new_v4(), 2);
        first.finish("initial sync", Ok(()));
        assert!(tracker.snapshot().folders.iter().any(|f| f.syncing));
        second.finish("initial sync", Ok(()));
        let mut changes = tracker.subscribe();
        assert_eq!(*changes.borrow_and_update(), tracker.snapshot().revision);
        assert!(!tracker.snapshot().folders.iter().any(|f| f.syncing));
        second.active();
        assert!(changes.has_changed().unwrap());
        second.finish("remote check", Err("connection failed".into()));
        assert!(!tracker.snapshot().folders.iter().any(|f| f.syncing));
        assert_eq!(
            tracker
                .snapshot()
                .folders
                .iter()
                .map(|f| f.errors.len())
                .sum::<usize>(),
            1
        );
    }
}
