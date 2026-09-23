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
   `RemoveSyncFolder`/`SetFolderEnabled`, the authentication methods
   (`IsAuthenticated`/`SignIn`/`SignOut`) and the `AuthenticationChanged`
   signal, emitted when the interactive sign-in flow finishes or the user
   signs out so clients don't have to poll.

Run it with:

```sh
cargo run -p gdrived
```

Set `GDRIVE_CLIENT_ID`/`GDRIVE_CLIENT_SECRET` in the environment beforehand
so the interactive OAuth flow can run on first use (see
[`gdrive-sync`'s auth module](../gdrive-sync/src/auth.rs)).

When installed as a package, `gdrived` normally isn't launched from your
shell - it's started on demand by D-Bus/systemd activation (see
`packaging/gdrived.service` and `packaging/org.gclient.GDrive1.service`),
which does **not** inherit your shell's exported environment variables.
For sign-in to work from `gdrive-ui` in that case, put your credentials in
`~/.config/gdrived/env` (create the file and directory if needed):

```sh
mkdir -p ~/.config/gdrived
cat > ~/.config/gdrived/env <<'EOF'
GDRIVE_CLIENT_ID=your-client-id
GDRIVE_CLIENT_SECRET=your-client-secret
EOF
chmod 600 ~/.config/gdrived/env
systemctl --user restart gdrived.service
```

Query it directly without the UI, e.g.:

```sh
busctl --user call org.gclient.GDrive1 /org/gclient/GDrive1 org.gclient.GDrive1 ListSyncFolders
```
