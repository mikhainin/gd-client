# gdrive-common

Shared types and utilities used by both `gdrived` and `gdrive-ui`:

- `config` — `AppConfig` and `SyncFolder`, the persisted TOML configuration
  model (`~/.config/gdrive-client/config.toml`). Each `SyncFolder` pairs one
  local directory with one Google Drive folder (or "My Drive" root).
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
- `error` — the shared `CommonError` type.
