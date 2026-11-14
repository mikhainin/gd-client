# gdrive-sync

The sync engine, used by the `gdrived` daemon.

- `engine` — `SyncEngine`: starts/stops a background task per `SyncFolder`
  (`start_folder`/`stop_folder`/`start_all`/`running_folders`), each of which
  does an initial recursive pull from Drive, then watches local changes and
  polls Drive's Changes API for remote changes, including newly created items.
  Live parent metadata resolves subtree membership, including moves into/out of
  the sync root; directory moves relocate descendant mappings together.
  Failed polls retain their cursor for replay after reconnect or restart.
  Same-name replacements retire old local mappings before downloading new IDs.
  Unsafe remote names, duplicate local-path mappings and occupied move targets
  are explicit errors rather than silent overwrites. Emits `SyncEvent`s
  (`FolderStarted`, `InitialSyncComplete`, `LocalChangeUploaded`,
  `RemoteChangeApplied`, `Error`) consumed by `gdrived` for logging.
  `sync_status` supplies authoritative runtime snapshots and `subscribe_status`
  supplies coalesced revision notifications for D-Bus forwarding. Per-folder
  activity covers initial reconciliation and actual application of local/remote
  work, not idle workers or empty polling. Outstanding errors are keyed by
  affected work; only successful reconciliation/retry of that work or explicit
  stop/restart clears them. Vanished local paths with no tracked remote item
  are reconciled as obsolete, including failed uploads followed by a rename;
  pending remote deletions and unrelated errors remain outstanding. Status
  mutations touch only the affected activity flag/error entry, without cloning
  the outstanding-error map. Initial sync retries connection failures and
  timeouts (including OAuth refresh transport failures) after the configured
  polling interval, with a five-second minimum in production. Each attempt
  persists the change cursor before the first recursive pull and retains it
  across retries, so deletions during a partial pull or retry wait are replayed
  by polling. No separate internet probe is used. Already-recorded downloads
  are skipped, and the completion count includes downloads from failed attempts.
  Waiting workers remain running but are not marked active; their error clears
  after initialization succeeds. Authentication, API, malformed-response and
  local failures remain fatal during initialization and visible after the
  task exits. Established workers already retry remote polls after failures,
  retaining the last successful change cursor. Stop/abort cancels pending
  retries and clears activity, and run generations reject updates
  from superseded tasks. Status is in memory, not persisted. Finished tasks
  are excluded from `running_folders`.
  Remote-deletion failures and local metadata failures other than a vanished
  path propagate as sync errors rather than being silently ignored.
  Local work is queued in SQLite and retried at the polling interval without
  requiring another filesystem event. Retries reconcile the latest disk state
  rather than replaying a stale create/delete event. Failures remain visible
  until that path succeeds; stopping a worker cancels attempts but preserves
  its queue for the next start. Pending paths are protected from startup pulls
  and remote polling; tracked local edits found before a remote write are
  queued instead of overwritten. Missing legacy checksum baselines are treated
  conservatively as local work, not evidence that a file is unchanged.
  Remote directory removal first checks descendant disk state and queues edited
  or untracked children. Deferred changes retain the previous change cursor
  until the blocking local work resolves, so reverting a failed local edit does
  not lose an intervening remote update.
  Local edits create a new Drive file before moving the previous file to Trash.
  Drive-generated IDs and staged upload metadata are persisted before retirement:
  a retry can recover a create whose response was lost, or finish a failed trash
  operation without uploading another copy. Retirement checks live replacement
  metadata rather than trusting the stored response. If that copy is gone or
  trashed, the previous copy is preserved and a fresh upload is queued.
  Local deletions also use Trash.
  MD5 checksums (via `md5`, only for Drive content matching, not cryptography)
  suppress uploads of unchanged files and daemon-generated download events.
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
  Token persistence writes a private sibling temporary file and atomically
  replaces the cache, so concurrent authentication readers see complete JSON.
  Interactive sign-in explicitly requests offline access and consent so Google
  issues a refresh token even after an earlier authorization. Missing offline
  access is reported as an error instead of saving a non-renewable session.
- `drive_client` — thin async wrapper around the Google Drive v3 REST API
  (list children, download/upload/create/trash, generated IDs, metadata recovery,
  list changes). Requests have
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
  Transport and deadline failures retain their source errors; OAuth connection,
  response, and body timeouts retain timeout classification through D-Bus.
  Drive JSON parsing happens only after the complete body is transferred:
  interrupted bodies retain a retryable transport classification, while invalid
  JSON/schema responses remain fatal. OAuth body transfers are likewise distinct
  from malformed token responses; source errors are preserved without body text.
  Create requests use pre-generated IDs rather than updating existing content
  in place. ID generation, metadata recovery, folder creation, and Trash updates
  have explicit 25-second HTTP budgets; uploads retain the transfer budget.
  Only a confirmed HTTP 404 is treated as an absent recovery target or an
  already-removed Trash target; other failures remain errors.
- `local_watcher` — filesystem change notifications (via `notify`) for a
  sync folder's local path. Read/access and metadata-only events, including
  ownership changes made by the daemon, are ignored; content changes remain
  queued and reconciled. The watcher callback persists paths independently of
  the transfer loop, before sending a wake-up notification. Queue revisions
  prevent finishing an older attempt from deleting newer events for that path.
  Paths retain literal filename characters, including Linux backslashes.
  The text-backed path store accepts UTF-8 names; unsupported byte sequences
  are explicit watcher errors, never lossy aliases to another file. Watcher
  errors and persistence failures remain visible until stop/restart.
- `state_db` — SQLite-backed (`rusqlite`) local state: per-folder Changes API
  page tokens and per-file sync state (Drive id, checksum, etc.), keyed by
  `sync_folder_id`. SQLite is bundled through rusqlite 0.42, including the
  WAL page-size validation fix required by RUSTSEC-2026-0459; no system SQLite
  upgrade is needed. `Connection` is wrapped in a `Mutex` since it isn't
  `Sync` but needs to be shared across `tokio::spawn`ed per-folder tasks.
  Pending local paths and replacement upload stages survive worker/daemon
  restarts. Removing a directory's mapping also removes descendant mappings.
- `ownership` — resolves configured owner user/group names to numeric
  uid/gid and applies them to synced files.

## Current limitations

There is no conflict-copy or reliable modified-time conflict arbitration.
The initial pull can overwrite existing local files and does not upload
local-only files; local watching starts only after it completes. Existing
tracked local edits and queued retries are preserved during reconciliation. Remote
polling discovers new remote items and follows remote moves/renames.
Local moves/renames are not reliably reconciled. Google Docs/Sheets/Slides
export is not implemented.

Local deletion of a tracked item moves it to Drive Trash. Local edits create
replacement files with new IDs before trashing the previous copies: Drive
links and per-file sharing do not automatically carry over, and both copies
can briefly remain live if retirement fails. Remote deletions still remove
local items. Drive Trash is not an independent backup; retain separate backups.
