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
  environment at runtime (letting anyone bring their own OAuth client),
  falling back to whatever was baked in at *build* time via the
  `GDRIVE_BUILD_CLIENT_ID`/`GDRIVE_BUILD_CLIENT_SECRET` build-environment
  variables (read with `option_env!`, never committed to source) - see
  `OAuthConfig::from_env` in `src/auth.rs`. Note: `oauth2`'s bundled
  `reqwest` HTTP client is disabled
  (`default-features = false`) to avoid a version conflict with the
  workspace's own `reqwest`; a small adapter (`reqwest_http_client`) wraps
  our `reqwest::Client` instead.
  `authorize` obtains tokens without persisting them so the daemon can coordinate
  completion with sign-out; `authenticate` remains the obtain-and-save wrapper.
  Access-token refresh reuses the OAuth adapter, preserves an omitted refresh
  token, and accepts a replacement refresh token when Google rotates it.
  Interactive sign-in explicitly requests offline access and consent so Google
  issues a refresh token even after an earlier authorization. Missing offline
  access is reported as an error instead of saving a non-renewable session.
- `drive_client` — thin async wrapper around the Google Drive v3 REST API
  (list children, download/upload/create/delete, list changes). Requests have
  a 10-second connection timeout and a 30-minute total HTTP timeout covering
  response headers and bodies, allowing large sync transfers. Folder browsing
  instead has a 25-second deadline across all pages: Drive filters out files
  and trashed items server-side, returning only folder ids/names. Pagination
  is preserved; failures/timeouts are errors, not empty or partial results.
  `list_child_folders` supplies the known folder MIME type with other metadata
  defaulted; sync's `list_children` still fetches all children and full sync
  metadata. Parent ids are escaped as Drive query literals.
  Every Drive operation shares token refresh: cached expiry triggers renewal
  30 seconds early, and HTTP 401 triggers at most one refresh/replay (including
  caches with unknown expiry). Concurrent operations share one refresh; tokens
  are persisted before installation. A sign-out or newer sign-in invalidates
  in-flight refreshes so they cannot overwrite or recreate the token cache.
  OAuth uses a separate prebuilt, no-redirect HTTP client with a 5-second
  connection and 15-second request/body limit; refresh including lock waiting
  is bounded to 20 seconds and remains inside the 25-second browse deadline.
  Missing client configuration, revoked refresh tokens, malformed responses,
  and cache-write failures are explicit errors, never empty listings. Refresh
  failures do not log credentials or response bodies.
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
