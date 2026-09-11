//! D-Bus service exposed by the daemon so the `gdrive-ui` Qt application (or
//! any other client, e.g. `busctl`) can inspect and control synchronisation
//! without talking to the engine/config directly.
//!
//! Bus name: `org.gclient.GDrive1`, object path: `/org/gclient/GDrive1`.

use std::path::PathBuf;
use std::sync::Mutex;

use gdrive_common::dbus_api::SyncFolderRow;
use gdrive_common::{AppConfig, SyncFolder};
use gdrive_sync::SyncEngine;
use uuid::Uuid;
use zbus::interface;

/// Shared daemon state backing the D-Bus interface: the persisted
/// configuration (guarded by a mutex since D-Bus calls are handled
/// concurrently) and the running sync engine.
pub struct GDriveService {
    config: Mutex<AppConfig>,
    config_path: PathBuf,
    engine: SyncEngine,
}

impl GDriveService {
    pub fn new(config: AppConfig, config_path: PathBuf, engine: SyncEngine) -> Self {
        Self {
            config: Mutex::new(config),
            config_path,
            engine,
        }
    }

    fn persist(&self, config: &AppConfig) -> zbus::fdo::Result<()> {
        config
            .save_to(&self.config_path)
            .map_err(|e| zbus::fdo::Error::Failed(format!("failed to save configuration: {e}")))
    }
}

/// Implements the `org.gclient.GDrive1` interface defined in
/// `gdrive_common::dbus_api` (see [`gdrive_common::dbus_api::GDrive1`] for
/// the client-side proxy consumed by `gdrive-ui`). The method
/// signatures here must stay in sync with that trait.
#[interface(name = "org.gclient.GDrive1")]
impl GDriveService {
    /// Lists all configured sync folders and whether each is currently active.
    async fn list_sync_folders(&self) -> Vec<SyncFolderRow> {
        let config = self.config.lock().unwrap();
        let running = self.engine.running_folders();
        config
            .sync_folders
            .iter()
            .map(|f| {
                (
                    f.id.to_string(),
                    f.display_name.clone(),
                    f.drive_folder_id.clone().unwrap_or_default(),
                    f.local_path.to_string_lossy().to_string(),
                    f.owner_user.clone(),
                    f.owner_group.clone(),
                    f.enabled,
                    running.contains(&f.id),
                )
            })
            .collect()
    }

    /// Adds a new sync folder and, if `enabled` is true, starts synchronising
    /// it immediately. Returns the new folder's generated id.
    #[allow(clippy::too_many_arguments)]
    async fn add_sync_folder(
        &self,
        display_name: String,
        drive_folder_id: String,
        local_path: String,
        owner_user: String,
        owner_group: String,
        enabled: bool,
    ) -> zbus::fdo::Result<String> {
        let mut folder = SyncFolder::new(display_name, PathBuf::from(local_path), owner_user, owner_group);
        folder.drive_folder_id = if drive_folder_id.is_empty() {
            None
        } else {
            Some(drive_folder_id)
        };
        folder.enabled = enabled;
        let id = folder.id;

        let mut config = self.config.lock().unwrap();
        config.sync_folders.push(folder.clone());
        self.persist(&config)?;
        drop(config);

        if enabled {
            self.engine.start_folder(folder);
        }

        Ok(id.to_string())
    }

    /// Removes a sync folder, stopping synchronisation for it first.
    async fn remove_sync_folder(&self, id: String) -> zbus::fdo::Result<()> {
        let folder_id = parse_uuid(&id)?;
        self.engine.stop_folder(folder_id);

        let mut config = self.config.lock().unwrap();
        config.sync_folders.retain(|f| f.id != folder_id);
        self.persist(&config)
    }

    /// Enables or disables a sync folder, starting/stopping its background
    /// synchronisation task accordingly.
    async fn set_folder_enabled(&self, id: String, enabled: bool) -> zbus::fdo::Result<()> {
        let folder_id = parse_uuid(&id)?;

        let mut config = self.config.lock().unwrap();
        let folder = config
            .sync_folders
            .iter_mut()
            .find(|f| f.id == folder_id)
            .ok_or_else(|| zbus::fdo::Error::Failed(format!("no such sync folder: {id}")))?;
        folder.enabled = enabled;
        let folder = folder.clone();
        self.persist(&config)?;
        drop(config);

        if enabled {
            self.engine.start_folder(folder);
        } else {
            self.engine.stop_folder(folder_id);
        }
        Ok(())
    }
}

fn parse_uuid(id: &str) -> zbus::fdo::Result<Uuid> {
    Uuid::parse_str(id).map_err(|e| zbus::fdo::Error::Failed(format!("invalid sync folder id {id:?}: {e}")))
}
