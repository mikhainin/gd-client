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
  for the interface's method names/signatures — both sides must match it
  exactly, since zbus does not check this across independent crates at
  compile time.
- `error` — the shared `CommonError` type.
