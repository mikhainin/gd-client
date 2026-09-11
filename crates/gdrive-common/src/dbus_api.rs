//! D-Bus interface contract shared between `gdrived` (which implements it as
//! a `zbus` service) and `gdrive-ui` (which consumes it as a `zbus` client
//! proxy). Keeping the bus name, object path and method signatures in one
//! place ensures both sides stay in sync.

/// Well-known session bus name `gdrived` registers.
pub const BUS_NAME: &str = "org.gclient.GDrive1";

/// Object path `gdrived` serves the [`GDrive1`] interface at.
pub const OBJECT_PATH: &str = "/org/gclient/GDrive1";

/// One row describing a configured sync folder, as returned by
/// [`GDrive1::list_sync_folders`]:
/// `(id, display_name, drive_folder_id, local_path, owner_user, owner_group, enabled, running)`.
pub type SyncFolderRow = (String, String, String, String, String, String, bool, bool);

/// The `org.gclient.GDrive1` D-Bus interface exposed by `gdrived`, letting
/// clients (e.g. `gdrive-ui`) list, add, remove and toggle sync folders
/// without touching the configuration file or sync engine directly.
#[zbus::proxy(
    interface = "org.gclient.GDrive1",
    default_service = "org.gclient.GDrive1",
    default_path = "/org/gclient/GDrive1"
)]
pub trait GDrive1 {
    /// Lists all configured sync folders and whether each is currently active.
    fn list_sync_folders(&self) -> zbus::Result<Vec<SyncFolderRow>>;

    /// Adds a new sync folder and, if `enabled` is true, starts synchronising
    /// it immediately. Returns the new folder's generated id.
    #[allow(clippy::too_many_arguments)]
    fn add_sync_folder(
        &self,
        display_name: &str,
        drive_folder_id: &str,
        local_path: &str,
        owner_user: &str,
        owner_group: &str,
        enabled: bool,
    ) -> zbus::Result<String>;

    /// Removes a sync folder, stopping synchronisation for it first.
    fn remove_sync_folder(&self, id: &str) -> zbus::Result<()>;

    /// Enables or disables a sync folder, starting/stopping its background
    /// synchronisation task accordingly.
    fn set_folder_enabled(&self, id: &str, enabled: bool) -> zbus::Result<()>;
}
