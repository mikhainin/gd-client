# gdrive-sync

The experimental sync engine used by the `gdrived` daemon. Use disposable data
when evaluating it and retain independent backups.

The nondefault `test-support` feature provides a disposable full-application
smoke endpoint override; see the [UI testing instructions](../gdrive-ui/README.md#hidden-full-application-smoke).
Only `http://<numeric loopback IP>:<explicit port>` origins are accepted through
`GDRIVE_TEST_DRIVE_ORIGIN`. In that mode the default HTTP timeout is five seconds
(existing explicit per-request metadata deadlines still apply), redirects and
proxies are disabled, and OAuth refresh is disabled. Normal builds
do not read this variable or expose endpoint redirection. Never enable this
feature in packaged binaries.

- `engine` — `SyncEngine`: starts/stops a background task per `SyncFolder`
  (`start_folder`/`stop_folder`/`start_all`/`running_folders`), each of which
  watches local changes before the first network request, performs an initial
  recursive merge from Drive, then reconciles local changes and
  polls Drive's Changes API for remote changes, including newly created items.
  Transfers are sequential within each folder; enabled folders have separate
  workers. A missing Drive folder ID selects "My Drive" (`root`).
  Enabled local roots must not overlap, including symlink/bind-mount aliases
  and backing trees reachable through nested mounts. Enabled folder UUIDs must
  also be unique, even for disjoint roots.
  The supported filesystem baseline is non-casefolded ext2/ext3/ext4, Btrfs,
  and tmpfs, including root ancestors; other filesystems fail explicitly rather
  than guessing filename equivalence. Descendant symlinks and special files are
  unsupported. Root-level aliases are resolved before worker admission.
  Startup blocks every affected binding before filesystem or Drive mutations,
  reports per-folder errors, and leaves unrelated bindings runnable. Root leases
  also reject overlapping direct `start_folder` calls and remain held until
  cancelled workers drop. Generations are registered before root resolution,
  so a stop during inspection prevents subsequent admission or mutation.
  Read-only worker resolution shares one inspection slot and a three-second
  queueing/execution deadline. A timed-out or cancelled lookup retains that slot
  until its blocking read exits, but cannot admit a root or mutate files.
  Resolution timeouts remain visible in status and retry at the polling interval,
  so a delayed lookup does not permanently stop otherwise valid bindings.
  Local mutations use non-detached blocking sections,
  yielding a multithreaded runtime's worker thread without allowing cancellation
  to release the root before the syscall finishes. Replacement workers wait in
  bounded two-second attempts for retiring roots, reporting timeouts and retrying
  at the polling interval. Removing/disabling a startup conflict rechecks only
  root-blocked bindings, without restarting unrelated workers or failures.
  A remaining enabled binding with a duplicate configured UUID retains its
  root-blocked retry marker when another occurrence is disabled.
  The daemon prepares root diagnostics off-lock and uses
  `start_all_inspected`/`reconcile_roots_inspected` to apply them under its
  configuration lock without filesystem inspection. Those callers must
  revalidate the inspected snapshot before applying it. Direct `start_all`
  and `reconcile_blocked_roots` inspect synchronously.
  Workers use the resolved local root. Local reads, directory creation, staging,
  publication, relocation, removal, and ownership changes walk pinned directory
  descriptors with Linux `openat2` no-symlink/beneath resolution. Parent-path
  replacement cannot redirect a subsequent syscall through a symlink.
  File reads pin and validate regular-file identity before opening data;
  ownership changes use the pinned inode. This requires Linux 5.6 or newer and
  accessible `/proc/self/fd`; there is no unsafe path-based fallback.
  Disable sync before moving a root, changing its ancestors, mount topology,
  or directory casefold settings. Descriptor pinning is not a filesystem
  transaction or coordination with external directory moves.
  Live parent metadata resolves subtree membership, including moves into/out of
  the sync root; directory moves relocate descendant mappings together.
  Failed polls retain their cursor for replay after reconnect or restart.
  Same-name replacements retire old local mappings before downloading new IDs.
  Startup checks live metadata before rejecting old mappings; moved-out items
  release their paths before same-path creations are replayed.
  Unsafe remote names and duplicate tracked local-path mappings are explicit
  errors rather than silent overwrites. Untracked occupied destinations are
  preserved as local conflict copies. Emits `SyncEvent`s
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
  updates are generation-scoped. Initial sync retries connection failures and
  timeouts (including OAuth refresh transport failures) after the configured
  polling interval, with a five-second minimum. The starting change cursor
  is persisted before the first recursive pull and retained across retries and
  restarts, so deletions during a partial pull or retry wait are replayed
  by polling. No separate internet probe is used. Already-recorded downloads
  are skipped, and the completion count includes downloads from failed attempts.
  Waiting workers remain running but are not marked active; their error clears
  after initialization succeeds. Authentication, API, malformed-response and
  local failures remain fatal during initialization and visible after the
  task exits. Established workers already retry remote polls after failures,
  retaining the last successful change cursor. Stop/abort cancels pending
  retries and clears activity. Status is in memory, not persisted. Finished tasks
  are excluded from `running_folders`.
  Remote-deletion failures and local metadata failures other than a vanished
  path propagate as sync errors rather than being silently ignored.
  Local work is queued in SQLite and retried at the polling interval without
  requiring another filesystem event. Retries reconcile the latest disk state
  rather than replaying a stale create/delete event. Failures remain visible
  until that path succeeds; stopping a worker cancels attempts but preserves
  its queue for the next start. Startup and periodic disk scans also discover
  local-only files, missing tracked paths, and entire locally moved directories.
  Existing local content is preserved during the initial merge. Tracked local
  edits found before a remote write are
  queued instead of overwritten. Missing legacy checksum baselines are treated
  conservatively as local work, not evidence that a file is unchanged.
  Remote directory removal first checks descendant disk state and preserves
  dirty subtrees as local conflict directories, queued recursively for upload.
  Deferred changes retain the previous change cursor
  until the blocking local work resolves, so reverting a failed local edit does
  not lose an intervening remote update.
  Local edits create a new Drive file and retain the previous live file under
  a deterministic `<name>.remote-history-<id-digest>` sibling name. The digest
  identifies the replacement Drive ID, not the previous contents, so retries use
  the same history name even if another client edits it. Each later replacement
  has a new identity; a no-op history name receives a distinct suffix.
  This metadata-only v3 rename
  leaves concurrent content edits on that same live ID; replacement uploads
  never send Trash requests for previous versions. A previous version already
  renamed by another client is left at its current name.
  Before retirement, live previous-file metadata is compared with the stored
  baseline. Concurrent remote content is downloaded to a deterministic
  `.remote-conflict-<content-digest>` local copy before retention (a UUID is used
  if that name already holds different bytes); failures leave the
  previous copy live and retryable. Conflict copies are themselves queued for
  upload. During initial binding, occupied local paths become
  `.local-conflict-<uuid>` siblings before remote content is installed.
  Local creates/updates run before deletions; a failed write postpones deletion
  of rename sources. Ordinary local moves use create-and-trash rather than
  preserving IDs; native-document links have the exception described below.
  Drive-generated IDs and staged upload metadata are persisted before retirement:
  a retry can recover a create whose response was lost, or finish a failed history rename
  operation without uploading another copy. Retirement verifies live replacement
  metadata; initial reconciliation recognizes staged IDs before collision checks.
  Conflict filenames are bounded to 255 bytes without splitting UTF-8 characters.
  Replacement metadata is never trusted solely from the stored response. If that copy is gone or
  trashed, the previous copy is preserved and a fresh upload is queued.
  Retained versions are ordinary remote files/folders: they remain discoverable
  under their history names and sync locally, including later edits to them.
  They keep their original IDs and sharing; Drive links and per-file sharing
  do not automatically carry over to the replacement. The replacement alone is
  bound to the original local path. Replacing a directory atomically releases
  its old descendant mappings and staged uploads, while preserving queued
  local notifications, so retries cannot resurrect or trash the retained subtree.
  Scanning and deletion preflight defer children beneath a directory replaced
  by a file until that parent replacement reconciles.
  History names use the same UTF-8-safe 255-byte bound as conflict names,
  reserving space for native documents' synthesized browser-link suffixes.
  Both versions can temporarily share the original name if history renaming
  fails; the persisted stage resumes that rename without another upload.
  There is no automatic history cleanup; history uses remote and local storage.
  Explicit local deletions move the tracked remote item to Drive Trash.
  MD5 content checksums (not a security mechanism) suppress uploads of unchanged
  files and daemon-generated download events.
- `google_document` — native Google Docs, Sheets and Slides are represented as
  `.gdoc.desktop`, `.gsheet.desktop` and `.gslides.desktop` browser links.
  These Linux desktop links contain only `Type=Link`, a display name and the
  document's HTTPS URL/ID, never executable commands. Names are escaped and
  IDs validated. Their generated content checksum and original MIME type are
  persisted, so unchanged links never echo as binary uploads. Discovery,
  remote renames and deletion use the same reconciliation as other files.
  Moving/renaming an unchanged link updates native document metadata and keeps
  its ID (unlike ordinary content replacements); retain its link extension.
  Deleting a tracked link moves the native document to Drive Trash.
  Link contents are read-only: local edits/copies return visible errors and
  never upload the pointer over the native document. Edit documents in the
  browser; these links do not cache document contents or provide offline editing.
  Link filenames and tracked MIME metadata identify pointer candidates; malformed
  moved links cannot fall back to ordinary uploads. Ordinary text containing an
  ID marker is not a link. Moves reject tracked destinations before contacting
  Drive, persist successful remote metadata before installation, and revalidate
  destination bytes after staging, preserving detected intervening edits/deletions.
  Browser-only document edits do not change link checksums or create conflict links.
- `auth` — Google OAuth2 flow (via the `oauth2` crate) and cached token
  storage. Reads `GDRIVE_CLIENT_ID`/`GDRIVE_CLIENT_SECRET` from the
  environment at runtime (letting anyone bring their own OAuth client),
  falling back to whatever was baked in at *build* time via the
  `GDRIVE_BUILD_CLIENT_ID`/`GDRIVE_BUILD_CLIENT_SECRET` build-environment
  variables (read with `option_env!`, never committed to source) - see
  `OAuthConfig::from_env` in `src/auth.rs`. Runtime overrides must supply both
  values; otherwise the build-time pair is used. Interactive sign-in uses
  PKCE, the full Drive scope, and a loopback redirect at `127.0.0.1:8734`.
  The redirect listener is bound before opening the browser, has an eight-minute
  consent deadline and five-second request/response I/O deadlines, validates
  the generated state, and decodes exactly one authorization code. Incomplete
  and oversized HTTP requests are rejected without ending consent. Failed,
  idle and invalid-state connections do not cancel sign-in; only errors carrying
  the matching state terminate it. The daemon additionally bounds
  the whole flow to ten minutes.
  The authorization-code exchange has a 15-second connection timeout and
  30-second request/body timeout, separate from refresh deadlines below.
  Note: `oauth2`'s bundled `reqwest` HTTP client is disabled
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
  (list children, download/upload/create/rename/trash, generated IDs, metadata recovery,
  list changes). Requests have
  a 10-second connection timeout and a 30-minute per-request HTTP timeout covering
  response headers and bodies. Folder browsing
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
  response, and body timeouts during refresh retain timeout classification
  through D-Bus. Interactive code exchange also retains timeout classifications
  and underlying sources without including token-response bodies in its message.
  Drive JSON parsing happens only after the complete body is transferred:
  interrupted bodies retain a retryable transport classification, while invalid
  JSON/schema responses are non-transport errors (fatal during initialization).
  OAuth refresh body transfers are likewise distinct
  from malformed token responses; source errors are preserved without body text.
  Create requests use pre-generated IDs rather than updating existing content
  in place. ID generation, metadata recovery, folder creation, history renames,
  native-document moves, and Trash updates have explicit 25-second HTTP budgets;
  uploads retain the transfer budget. These per-request budgets do not include
  token refresh or a 401 replay; folder browsing has the overall deadline above.
  Only a confirmed HTTP 404 is treated as an absent recovery target or an
  already-removed Trash target; other failures remain errors.
- `local_watcher` — filesystem change notifications (via `notify`) for a
  sync folder's local path. Individual nonrecursive registrations cover live
  directories only; registration refreshes exclude recovery/staging trees and
  remove watches when directories move into history. Device/inode changes also
  replace stale registrations when a new live directory reuses the same path.
  New live directories are picked up between engine iterations, with periodic
  scans covering missed notifications. Read/access and metadata-only events, including
  ownership changes made by the daemon, are ignored; content changes remain
  queued and reconciled. The watcher callback persists paths independently of
  the transfer loop, before sending a wake-up notification. Queue revisions
  prevent finishing an older attempt from deleting newer events for that path.
  Paths retain literal filename characters, including Linux backslashes.
  The text-backed path store accepts UTF-8 names; unsupported byte sequences
  are explicit watcher errors, never lossy aliases to another file. Callback
  errors and persistence failures remain visible until stop/restart; registration
  errors clear only after a successful registration refresh.
  Downloads are staged and atomically renamed; `.gdrive-download-` filename
  prefixes are reserved internal staging paths and excluded from watching/scans.
  Filtering is relative to the sync root. Staging preserves destination permission
  bits, including executable bits; new downloads are private by default. Queued
  watcher wakeups are coalesced before scanning, without dropping watcher errors.
- `state_db` — SQLite-backed (`rusqlite`) local state: per-folder Changes API
  page tokens and per-file sync state (Drive id, checksum, etc.), keyed by
  `sync_folder_id`. SQLite is bundled through rusqlite 0.42, including the
  WAL page-size validation fix required by RUSTSEC-2026-0459; no system SQLite
  upgrade is needed. `Connection` is wrapped in a `Mutex` since it isn't
  `Sync` but needs to be shared across `tokio::spawn`ed per-folder tasks.
  Pending local paths and replacement upload stages survive worker/daemon
  restarts. Removing a directory's mapping also removes descendant mappings.
- `ownership` — resolves configured owner user/group names to numeric
  uid/gid, falling back to the daemon process's user/group for empty or
  unresolved names, and applies them to synced files. Ownership failures
  propagate as sync errors; the daemon needs permission to set the chosen owner.

## Current limitations

Conflict handling uses the retention/conflict-copy rules above rather than
choosing a winner by modified time. Native documents use browser links, not
Office/PDF exports; other native Google file types and Drive shortcuts are not
implemented.

Uploads are not resumable; uploads and downloads buffer whole files in memory.
Local replacement downloads and clean remote removals retain the displaced
file/directory in its parent's `.gdrive-recovery/<entry-id>/<original-name>`.
Renames retain the original inode, including subsequent writes through existing
open handles; publication and remote moves use Linux `RENAME_NOREPLACE` and fail
closed rather than overwriting a concurrently created destination. Detected dirty
paths still use the visible conflict-copy workflow described above.
Replacement/removal/move intent is synced before changing paths. Scans and remote
polls reconcile it before processing missing tracked paths: restore an absent
original without overwrite, or keep an already-published/new destination.
Removals and relocations consult committed database mappings so completed
operations are not undone or reuploaded after a crash. Move journals capture the
original mapping's unique state version; each metadata upsert/relocation changes
that version atomically, so a newly rebound path is never mistaken for the old
mapping, even when its Drive ID and metadata are unchanged. SQLite uses
full-synchronous WAL commits. Traversal skips vanished queued descendants after
a directory restore but reports a missing root and other I/O failures.
Restores verify recorded
device/inode identity and fail explicitly if the expected source changed or
disappeared. A daemon stop or failed publication is not inferred as user deletion
of the live Drive item. Marker/journal reads reject non-regular files.
Recovery operations use the same descriptor-rooted, no-symlink filesystem
operations as live synchronization, including bounded regular-file metadata reads.
Unsupported or inaccessible destinations remain errors and do not clear intents.
Each store's owner marker includes the configured folder UUID; moving a store
between bindings fails explicitly rather than interpreting its root-relative
journals against another binding's mappings.
Recovery directories are private, excluded from scans/uploads, and never cleaned
automatically. Restore entries manually to an ordinary path after pausing sync.
The owner marker is synced in an excluded `.gdrive-recovery-init-<id>` staging
directory before no-replace publication; an interrupted/failed initialization
cannot publish an incomplete recovery store or block later retries.
The name `.gdrive-recovery` is reserved at every depth: unmanaged existing paths
and remote items with that name cause an explicit error, not silent adoption.
The `.gdrive-download-` and `.gdrive-recovery-init-` prefixes are likewise reserved
for internal staging and rejected in remote names.
Unsupported no-replace renames and cross-filesystem moves are errors, never
copy-and-delete fallbacks. Recovery consumes local disk and is not an independent
backup or protection against disk failure.

Remote metadata writes are not conditional transactions. Replacement retention
avoids the check-then-Trash content race without relying on unsupported v3
conditional-write headers or the v2 API.
Concurrent renames can still cause naming conflicts, which are reported rather
than resolved by choosing an arbitrary ID. Explicit local deletions still trash
their tracked remote items and are not transactional multi-client arbitration.
Remote deletions move unchanged local items into recovery; detected local edits are preserved
as conflict copies. Drive Trash and retained history are not independent backups.
