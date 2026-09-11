//! Application configuration: the set of synchronised folders and
//! global settings, persisted as TOML under the user's config directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::CommonError;
use crate::paths;

/// A single "what to sync" entry configured by the user through the UI.
///
/// Maps one Google Drive folder to one local directory, and records which
/// local user/group should own the synchronised files on disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncFolder {
    /// Stable identifier, generated once when the folder is added. Used to
    /// key persistent sync-state (see `gdrive-sync`'s SQLite store) so that
    /// renaming a folder doesn't lose sync history.
    pub id: Uuid,

    /// Human readable name shown in the UI (defaults to the Drive folder name).
    pub display_name: String,

    /// The Google Drive folder id (the `id` field from the Drive API), or
    /// `None` for "My Drive" root.
    pub drive_folder_id: Option<String>,

    /// Local destination directory that this Drive folder is mirrored into.
    pub local_path: PathBuf,

    /// The local system user that should own synchronised files.
    /// Defaults to the current user running the service.
    pub owner_user: String,

    /// The local system group that should own synchronised files.
    /// Defaults to the current user's primary group.
    pub owner_group: String,

    /// Whether this folder is currently active (synced) or paused.
    pub enabled: bool,
}

impl SyncFolder {
    pub fn new(display_name: impl Into<String>, local_path: PathBuf, owner_user: impl Into<String>, owner_group: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            display_name: display_name.into(),
            drive_folder_id: None,
            local_path,
            owner_user: owner_user.into(),
            owner_group: owner_group.into(),
            enabled: true,
        }
    }
}

/// Top-level application configuration, persisted at
/// `$XDG_CONFIG_HOME/gdrive-client/config.toml` (see [`paths::config_file`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AppConfig {
    /// All configured local<->Drive folder synchronisations.
    #[serde(default)]
    pub sync_folders: Vec<SyncFolder>,

    /// Polling interval (seconds) used to check Drive for remote changes.
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
}

fn default_poll_interval_secs() -> u64 {
    60
}

impl AppConfig {
    /// Loads the configuration from the default location, returning a
    /// default (empty) configuration if the file does not exist yet.
    pub fn load() -> Result<Self, CommonError> {
        let path = paths::config_file()?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self, CommonError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        let config = toml::from_str(&text)?;
        Ok(config)
    }

    /// Persists the configuration to the default location, creating parent
    /// directories as needed.
    pub fn save(&self) -> Result<(), CommonError> {
        let path = paths::config_file()?;
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), CommonError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let mut cfg = AppConfig::default();
        cfg.sync_folders.push(SyncFolder::new(
            "My Photos",
            PathBuf::from("/home/user/GoogleDrive/Photos"),
            "user",
            "user",
        ));

        let dir = std::env::temp_dir().join(format!("gdrive-client-test-{}", Uuid::new_v4()));
        let path = dir.join("config.toml");
        cfg.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(cfg, loaded);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_file_yields_default() {
        let loaded = AppConfig::load_from(Path::new("/nonexistent/gdrive-client/config.toml")).unwrap();
        assert_eq!(loaded, AppConfig::default());
    }
}
