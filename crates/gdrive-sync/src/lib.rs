//! Synchronisation engine: watches local folders for changes, polls Google
//! Drive for remote changes, and reconciles the two using a local SQLite
//! state store to track what has already been synced.

pub mod auth;
pub mod drive_client;
pub mod engine;
pub mod error;
pub mod local_watcher;
pub mod ownership;
pub mod state_db;

pub use engine::{SyncEngine, SyncEvent};
pub use error::SyncError;
