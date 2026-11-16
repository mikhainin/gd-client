# gdrive-common

Shared types and utilities used by `gdrived`, `gdrive-ui`, and `gdrive-sync`:

- `config` — `AppConfig` and `SyncFolder`, the persisted TOML configuration
  model.
- `paths` — XDG-based filesystem locations (config file, data directory,
  sync-state DB, cached OAuth token). Linux defaults are
  `~/.config/gdrive-client/config.toml` and `~/.local/share/gdrive-client/`
  containing `sync-state.sqlite3` and `oauth-token.json`. Resolving a data
  path also creates the data directory.
- `local_roots` — read-only root resolution and overlap diagnostics shared by
  configuration validation and worker admission.
- `error` — `CommonError` covers missing home directories, I/O, and TOML
  parsing/serialization, preserving underlying error sources.

## Configuration

Each `SyncFolder` pairs one local directory with one Google Drive folder
(`drive_folder_id = None` means "My Drive" root), with a stable UUID, display
name, owner user/group, and enabled flag. `SyncFolder::new` generates a UUID,
selects root, and enables the folder; the caller supplies the path and ownership
strings. The optional `drive_folder_path` stores a selection-time display path
such as `My Drive/Parent/Photos`; syncing still uses `drive_folder_id`.
Older configurations without a path remain loadable, and absent paths are
omitted when saving. Stored paths do not track remote moves or renames.

`AppConfig::load`/`save` use the standard config location; `load_from`/`save_to`
accept explicit paths. Saving writes pretty TOML to a private, same-directory
temporary file, syncs it, atomically replaces the configuration, and syncs the
parent directory. Failures before replacement leave the previous file intact;
a directory-sync failure after replacement is reported, but the file is already
the complete new configuration. Loading a missing file returns `AppConfig::default()`;
malformed or unreadable files return errors rather than being reset.
These helpers perform deserialization, not semantic validation of paths,
ownership, folder overlap, or polling values. `validate_local_roots` separately
rejects equal/nested enabled roots, including symlink and bind-mount aliases,
and duplicate enabled folder UUIDs even when their paths are disjoint.
Linux mount information and descriptor mount IDs map each root and any nested
mounts to their backing filesystem paths; unavailable or inconsistent mount
information fails closed. Overlapping mount footprints within one root are
rejected too: recursive bind
aliases or duplicate mounted subtrees must not create traversal cycles or give
one inode multiple live synchronization paths. Disjoint mounted trees remain
supported. Mount paths are decoded as raw filesystem bytes, so
an unrelated non-UTF-8 mount name does not prevent validation. Descriptor mount
inspection uses `O_PATH`, never a data-reading open that could block on a FIFO.
Existing directory
components are canonicalized before normalizing `..`; missing suffixes are
allowed without creating anything. Dangling symlinks, non-directory components,
and inaccessible paths fail explicitly with a preserved I/O source. Disabled
bindings may overlap but must pass validation before they can be enabled.
Case-insensitive aliases are not assumed distinct: existing directory components
must be on the verified ext2/ext3/ext4, Btrfs, or tmpfs baseline, without the
kernel casefold flag. FAT/exFAT, casefolded directories, and unverified
filesystems are rejected explicitly, including canonical symlink-target
ancestors and ancestors of missing suffixes.
Directory-attribute inspection failures fail closed; older tmpfs kernels that
lack both casefolding and the attribute ioctl are recognized separately.
Runtime filesystem operations share these checks, including directories created
or encountered below an admitted root. Validation never creates case-probe files.

Omitted TOML `sync_folders` defaults to an empty list and omitted
`poll_interval_secs` defaults to 60, also for `AppConfig::default()` and a missing
configuration file. The sync engine clamps explicit intervals to at least 5 seconds.
Other than the optional Drive ID/path, folder fields must be present in TOML.

## D-Bus contract

- `dbus_api` — the `org.gclient.GDrive1` D-Bus interface contract: bus name,
  object path `/org/gclient/GDrive1`, and a `#[zbus::proxy]` trait (`GDrive1`)
  generating `GDrive1Proxy` for session-bus clients. `gdrived` implements the
  service separately, so both sides must match
  [the shared signatures](src/dbus_api.rs); zbus does not check this across
  independent crates at compile time. The contract includes
  `AuthenticationChanged(b)` after successful sign-in/out and `SignInFailed(s)`
  for asynchronous sign-in failure, with a credential-free retry message. Clients query
  `IsAuthenticated` for initial/reconnected cached-token state, not a live
  credential-validity check. The workspace uses zbus 6's asynchronous
  proxy/service APIs on Tokio. The shared trait does not configure call
  deadlines; clients must bound their connections and calls.
  `SyncFolderRow` includes configuration plus separate `enabled` and `running`
  flags; `DriveFolderRow` is an `(id, name)` pair.
  The `SyncStatus` method returns a typed snapshot (`SyncStatus` with
  `FolderSyncStatus` entries) containing a
  monotonic revision and per-folder activity/outstanding error details;
  `folder_id` identifies the configured sync folder, not the remote Drive
  folder. Activity is separate from a worker merely running.
  `SyncStatusChanged(t)` invalidates the snapshot. Revisions are scoped to one daemon
  lifetime/bus owner. Clients subscribe before reading, re-read after
  invalidation, poll to recover lost notifications, and reset revision tracking
  on owner change. `AddSyncFolder` takes `drive_folder_path` immediately after
  `drive_folder_id`; `ListSyncFolders` rows include it in the same position.
  Empty or `root` input IDs select My Drive and are returned as empty IDs;
  an empty path means an unknown display path. UI and daemon wire signatures
  must be upgraded together, with the daemon restarted.

See [gdrived](../gdrived/README.md) for service behavior and authentication.
