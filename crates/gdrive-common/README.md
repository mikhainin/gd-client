# gdrive-common

Shared types and utilities used by both `gdrived` and `gdrive-ui`:

- `config` — `AppConfig` and `SyncFolder`, the persisted TOML configuration
  model (`~/.config/gdrive-client/config.toml`). Each `SyncFolder` pairs one
  local directory with one Google Drive folder (or "My Drive" root).
  The optional `drive_folder_path` stores a selection-time display path such
  as `My Drive/Parent/Photos`; syncing still uses `drive_folder_id`.
  Older configurations without a path remain loadable. Stored paths are
  not automatically updated after remote moves or renames.
- `paths` — XDG-based filesystem locations (config file, data directory,
  sync-state DB, cached OAuth token).
- `dbus_api` — the `org.gclient.GDrive1` D-Bus interface contract: bus name,
  object path, and a `#[zbus::proxy]` trait (`GDrive1`) implemented by
  `gdrived` and consumed by `gdrive-ui`. This is the single source of truth
  for the interface's method and signal names/signatures, including
  `AuthenticationChanged(b)` after successful sign-in/out. Clients query
  `IsAuthenticated` for initial/reconnected state. Both sides must match it
  exactly, since zbus does not check this across independent crates at
  compile time. The workspace uses zbus 6's asynchronous proxy/service APIs
  on Tokio; D-Bus names and wire types now come from zbus itself rather than
  the standalone zbus_names/zvariant crates.
  The additive `SyncStatus` method returns a typed snapshot containing a
  monotonic revision and per-folder activity/outstanding error details;
  `SyncStatusChanged(t)` invalidates it. Revisions are scoped to one daemon
  lifetime/bus owner. Clients subscribe before reading, re-read after
  invalidation, poll to recover lost notifications, and reset revision tracking
  on owner change. `AddSyncFolder` takes `drive_folder_path` immediately after
  `drive_folder_id`; `ListSyncFolders` rows include it in the same position.
  An empty path means an unknown display path. These changed wire signatures
  require upgrading the UI and daemon together and restarting the daemon.
- `error` — the shared `CommonError` type.
