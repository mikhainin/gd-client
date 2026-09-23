# gdrive-sync

The sync engine, used by the `gdrived` daemon.

- `engine` — `SyncEngine`: starts/stops a background task per `SyncFolder`
  (`start_folder`/`stop_folder`/`start_all`/`running_folders`), each of which
  does an initial recursive pull from Drive, then watches local changes and
  polls Drive's Changes API for remote changes. Emits `SyncEvent`s
  (`FolderStarted`, `InitialSyncComplete`, `LocalChangeUploaded`,
  `RemoteChangeApplied`, `Error`) consumed by `gdrived` for logging.
- `auth` — Google OAuth2 flow (via the `oauth2` crate) and cached token
  storage. Reads `GDRIVE_CLIENT_ID`/`GDRIVE_CLIENT_SECRET` from the
  environment, falling back to `GDRIVE_BUILD_CLIENT_ID`/
  `GDRIVE_BUILD_CLIENT_SECRET` *build-time* environment variables (read via
  `option_env!`, never hardcoded/committed to source) so a
  maintainer/packager can ship a pre-configured build needing no per-user
  setup, following the standard "installed application" OAuth pattern
  (e.g. `rclone`) — the client secret isn't confidential in this flow since
  PKCE protects it. Note: `oauth2`'s bundled `reqwest` HTTP client is disabled
  (`default-features = false`) to avoid a version conflict with the
  workspace's own `reqwest`; a small adapter (`reqwest_http_client`) wraps
  our `reqwest::Client` instead. Both this client and `drive_client`'s set
  explicit `connect_timeout`/`timeout` (see AGENTS.md's "Network and IPC
  calls must always have a timeout" rule) — a bare `reqwest::Client::new()`
  has no timeout and can hang indefinitely.
- `drive_client` — thin async wrapper around the Google Drive v3 REST API
  (list children, download/upload/create/delete, list changes). Its
  `reqwest::Client` sets `connect_timeout(15s)`/`timeout(120s)` for the
  same reason.
- `local_watcher` — filesystem change notifications (via `notify`) for a
  sync folder's local path.
- `state_db` — SQLite-backed (`rusqlite`) local state: per-folder Changes API
  page tokens and per-file sync state (Drive id, checksum, etc.), keyed by
  `sync_folder_id`. `Connection` is wrapped in a `Mutex` since it isn't
  `Sync` but needs to be shared across `tokio::spawn`ed per-folder tasks.
- `ownership` — resolves configured owner user/group names to numeric
  uid/gid and applies them to synced files.

Conflict resolution is currently "last writer wins" based on modified time;
directory moves/renames are handled as remove+recreate.
