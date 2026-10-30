# g-client

A small Google Drive sync suite for Linux, split into a background daemon
and a Qt6/QML front-end, built as a Rust workspace.

Choose local directories to pair with Google Drive folders or "My Drive".
The daemon handles syncing; the UI manages folders, sign-in, and status.

## Current limitations

**Experimental: use disposable folders and keep independent backups.**
Sync can overwrite local files, and deleting tracked local items permanently
deletes them from Drive, bypassing the trash. Remote discovery, conflict
handling, moves/renames, and Google document export are incomplete; see
[sync limitations](crates/gdrive-sync/README.md#current-limitations).

## Building

Requires Rust 1.89+, a C++ build toolchain, and Qt6 development libraries
and QML modules. On Debian/Ubuntu, run
[`scripts/install-qt6.sh`](scripts/install-qt6.sh) yourself to install the
system prerequisites (it invokes `sudo apt`).

Build Debian packages and install them from the repository root:

```sh
cargo install cargo-deb
make deb
sudo apt install ./target/debian/*.deb
```

For development, use `cargo build --workspace` and `cargo test --workspace`.
Without Qt, build only the daemon with `cargo build -p gdrived`.
See the [`Makefile`](Makefile) for release/package targets and the
[UI README](crates/gdrive-ui/README.md#ui-testing) for UI-specific tests.

## Running

Unless your build embeds an OAuth client, configure both `GDRIVE_CLIENT_ID`
and `GDRIVE_CLIENT_SECRET` for the daemon. Packaged installs use
`~/.config/gdrived/env`, not shell exports. See
[OAuth setup](crates/gdrived/README.md#oauth-client-configuration) for Google
Cloud requirements, runtime overrides, and build-time defaults.

Launch **g-client Google Drive Sync** from the application menu or run
`gdrive-ui`; packaged installs activate the daemon on demand. From source,
run these in separate terminals in your desktop session:

```sh
cargo run -p gdrived
cargo run -p gdrive-ui
```

Click **Sign in**, complete browser consent, then add a local path and
select a Drive folder (or leave its id empty for "My Drive"). New folders
are enabled immediately. Startup alone does not initiate sign-in.
Closing the window hides it to the tray; **Quit** exits only the UI.

## Data locations

- Config: `~/.config/gdrive-client/config.toml`
- Sync database and OAuth token: `~/.local/share/gdrive-client/`

These respect `XDG_CONFIG_HOME` and `XDG_DATA_HOME`; the packaged daemon's
OAuth environment file does not.

## Documentation

| Component | Details |
| --- | --- |
| [`gdrived`](crates/gdrived) | Daemon, OAuth setup, D-Bus control, diagnostics |
| [`gdrive-ui`](crates/gdrive-ui) | Qt6/QML UI, tray status, testing |
| [`gdrive-sync`](crates/gdrive-sync) | Sync engine, auth, Drive API, limitations |
| [`gdrive-common`](crates/gdrive-common) | Shared configuration, paths, D-Bus contract |

See [`AGENTS.md`](AGENTS.md) for contributor guidance and license checks.

Alternatives include [rclone](https://rclone.org/),
[Insync](https://www.insynchq.com/), [odrive](https://www.odrive.com/),
and the Google Drive web app.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
