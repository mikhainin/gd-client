# gdrived

Background daemon binary.

On startup it:

1. Loads the persisted configuration (`~/.config/gdrive-client/config.toml`).
2. Loads a cached OAuth2 access token, without starting an interactive flow.
   If none is available, it still starts the D-Bus service so `gdrive-ui`
   can request sign-in.
3. Starts the `gdrive_sync::SyncEngine` for every enabled sync folder when
   a cached token is available.
4. Serves the `org.gclient.GDrive1` interface (defined in
   `gdrive_common::dbus_api`, implemented in `dbus_service.rs`) on the
   session D-Bus bus, exposing `ListSyncFolders`/`AddSyncFolder`/
   `RemoveSyncFolder`/`SetFolderEnabled`/`IsAuthenticated`/`SignIn`/
   `SignOut`/`ListDriveFolders`/`SyncStatus`. `SignIn` spawns a background task running
   `gdrive_sync::auth::authorize`'s PKCE flow via `xdg-open`, so the
   D-Bus call itself returns immediately rather than blocking on browser
   interaction; `SignOut` deletes the cached token and stops all running
   folders. Successful sign-in and sign-out emit `AuthenticationChanged(b)`.

`SyncStatus` returns the engine's authoritative in-memory per-folder activity
and outstanding failures, with a monotonic revision. `SyncStatusChanged(t)`
notifies clients to re-read it. Subscribers are established before workers
start; snapshots remain available even if no UI was connected during a change.
Coalesced notifications do not lose the current state, and emission failures
are logged with a two-second bound. Clients can recover by reading a snapshot.
Startup bus connection/registration has a five-second overall bound.
The separate sync-event logging loop tolerates lag instead of terminating.
Idle background workers and empty polling are not active file sync. Errors
survive unrelated successes and clear only when affected work recovers or
the folder is explicitly stopped/restarted.

The OAuth flow has a ten-minute overall deadline, and a duplicate `SignIn`
request is rejected while one is pending. Its tokens are saved only when the
current attempt completes. Completion reads the live configuration and starts
enabled folders while holding the same state lock used by folder mutations,
so a concurrent disable/remove cannot be undone by a stale sign-in snapshot.
`SignOut` cancels the pending attempt and invalidates late completions before
deleting tokens. It awaits cancellation for up to two seconds to release the
OAuth loopback listener before another sign-in can start. If cleanup exceeds
that deadline, sign-out reports a timeout and new sign-ins remain rejected
until the aborted task has actually finished. Signals are emitted in state-change order, with a two-second
emission deadline; delivery failures are logged and clients can recover by
querying `IsAuthenticated`.

Cached OAuth expiry and refresh tokens are retained at startup and after sign-in.
All Drive calls renew expired credentials automatically, persist renewed tokens,
and retry an unauthorized request once. OAuth client credentials are needed for
refresh as well as initial sign-in, using the same runtime/build-time sources.
Revoked or missing refresh tokens return an actionable sign-out/sign-in error.
Such sessions need one fresh browser consent; token refresh cannot repair a
revoked grant. Interactive sign-in requests offline access for a renewable session.

`ListDriveFolders` browses direct, non-trashed Drive subfolders, fetching all
pages with a server-side folder filter. Its whole-operation timeout is
27 seconds (the underlying Drive browse deadline is 25 seconds), leaving
headroom for the UI's 30-second deadline. Missing authentication returns
`org.freedesktop.DBus.Error.AccessDenied`, timeouts return
`org.freedesktop.DBus.Error.Timeout`, and API/transport failures return
`org.freedesktop.DBus.Error.Failed` with diagnostic context. Only a successful
empty listing returns an empty folder list; failures never masquerade as one.
Expired/revoked credentials that cannot be renewed also return `AccessDenied`.

For diagnosis, set
`RUST_LOG=warn,gdrived=trace,gdrive_sync=trace,reqwest::connect=debug`.
Application diagnostics report cache expiry/configuration availability, listing
page/status/count, and refresh progress without tokens, OAuth response bodies,
or folder names. Avoid global HTTP trace logging, which can expose credentials.

Authentication and configuration regressions use temporary state and tokens:

```sh
cargo test -p gdrived
dbus-run-session -- env GDRIVE_UI_TEST_BUS=1 cargo test -p gdrived -- --ignored
```

The private-bus case verifies status snapshots/signals, fatal sync failures,
explicit stop, sign-out signals and cancellation without a real
Google account or the user's daemon.

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
## Packaged (systemd/D-Bus-activated) daemon

When installed from the `.deb`, `gdrived` is started via D-Bus activation
(`packaging/org.gclient.GDrive1.service`, which has
`SystemdService=gdrived.service` so activation routes through the systemd
user unit rather than a bare `Exec=`) or the systemd user unit directly
(`packaging/gdrived.service`). Such activation does not inherit the
invoking user's exported shell environment variables, so
`GDRIVE_CLIENT_ID`/`GDRIVE_CLIENT_SECRET` set in e.g. `~/.bashrc` are not
visible to it. To override OAuth credentials for the packaged daemon, set
them in `~/.config/gdrived/env` (referenced via
`EnvironmentFile=-%h/.config/gdrived/env` in `gdrived.service`; the leading
`-` makes the file optional).

Query it directly without the UI, e.g.:

```sh
busctl --user call org.gclient.GDrive1 /org/gclient/GDrive1 org.gclient.GDrive1 ListSyncFolders
```
