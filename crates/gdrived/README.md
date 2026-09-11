# gdrived

Background daemon binary.

On startup it:

1. Loads the persisted configuration (`~/.config/gdrive-client/config.toml`).
2. Tries to obtain a cached or fresh OAuth2 access token. If this fails
   (e.g. first run, before authenticating), it logs a warning but still
   starts the D-Bus service, so `gdrive-ui` can always connect — sync
   folders simply won't run until `gdrived` is restarted after
   authenticating.
3. Starts the `gdrive_sync::SyncEngine` for every enabled sync folder.
4. Serves the `org.gclient.GDrive1` interface (defined in
   `gdrive_common::dbus_api`, implemented in `dbus_service.rs`) on the
   session D-Bus bus, exposing `ListSyncFolders`/`AddSyncFolder`/
   `RemoveSyncFolder`/`SetFolderEnabled`.

Run it with:

```sh
cargo run -p gdrived
```

Set `GDRIVE_CLIENT_ID`/`GDRIVE_CLIENT_SECRET` in the environment beforehand
so the interactive OAuth flow can run on first use (see
[`gdrive-sync`'s auth module](../gdrive-sync/src/auth.rs)).

Query it directly without the UI, e.g.:

```sh
busctl --user call org.gclient.GDrive1 /org/gclient/GDrive1 org.gclient.GDrive1 ListSyncFolders
```
