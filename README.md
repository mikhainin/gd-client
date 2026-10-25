# g-client

A small Google Drive sync suite for Linux, split into a background daemon
and a Qt6/QML front-end, built as a Rust workspace.

## Motivation

Google ships official Google Drive clients for Windows and macOS, but not
for Linux. There are good general-purpose tools and third-party apps that
can fill the gap, but sometimes you just want something small and focused.

`g-client` is that: a lightweight background daemon plus a simple native Qt
UI, doing one job — keeping a few chosen local folders and Drive folders in
sync.

## Crates

| Crate | Description |
| --- | --- |
| [`gdrive-common`](crates/gdrive-common) | Shared config model (`AppConfig`, `SyncFolder`), filesystem paths, and the `org.gclient.GDrive1` D-Bus interface contract. |
| [`gdrive-sync`](crates/gdrive-sync) | The sync engine: OAuth2 auth, Drive API client, local filesystem watcher, SQLite-backed state tracking, and file ownership handling. |
| [`gdrived`](crates/gdrived) | Background daemon binary. Loads configuration, runs the sync engine for every enabled folder, and exposes control over D-Bus. |
| [`gdrive-ui`](crates/gdrive-ui) | Qt6/QML desktop UI (via [cxx-qt](https://github.com/KDAB/cxx-qt)) that talks to `gdrived` over D-Bus to list/add/remove/enable sync folders. |

Each sync folder pairs one local directory with one Google Drive folder
(or "My Drive" root). `gdrived` runs the sync workers for enabled folders
while signed in; `gdrive-ui` is a control panel and holds no sync state itself.

## Current limitations

The sync engine is still a foundation, not a complete backup or conflict-safe
mirror. Start with a disposable folder and keep independent backups:

- Each worker starts with a recursive download from Drive. It can overwrite
  existing local files; it does not initially upload local-only files.
- Local changes are watched after that initial download. Remote polling
  updates already-tracked items, but skips newly created, untracked Drive
  items until the worker is restarted (for example, by disabling and
  re-enabling the folder).
- Removing a tracked local item permanently deletes it from Drive, bypassing
  the trash. Remote deletions also remove the corresponding local items.
- There is no conflict-copy or reliable modified-time conflict arbitration.
  Moves/renames are not reliably reconciled, and Google Docs/Sheets/Slides
  export is not implemented.

## Building

Requires Rust 1.89 or newer (edition 2021, `rust-version = "1.89"`
in the workspace `Cargo.toml`), a C++ build toolchain, and Qt6 development
libraries and QML runtime modules for `gdrive-ui`.

On Debian/Ubuntu, [`scripts/install-qt6.sh`](scripts/install-qt6.sh) lists
and installs the build tools and Qt packages. Run it yourself on your
development machine; it invokes `sudo apt`. `kdialog` is optional for the
native local-folder picker; an in-app picker is available without it.

```sh
cargo build --workspace
cargo test --workspace
```

Without Qt, build and test just the daemon and its dependencies with
`cargo build -p gdrived` and
`cargo test -p gdrive-common -p gdrive-sync -p gdrived`.
Additional UI test prerequisites and commands are in
[`gdrive-ui`'s README](crates/gdrive-ui/README.md#ui-testing).

### Debian packages

Install [`cargo-deb`](https://github.com/kornelski/cargo-deb) with
`cargo install cargo-deb`, then run `make deb` to build both packages into
`target/debian/`.

The root [`Makefile`](Makefile) provides `make build` (a **release** workspace
build), `make test` (workspace Rust tests), and `make clean` (remove build
artifacts). `make deb` first builds in release mode, then runs
`cargo deb -p gdrived --no-build` and `cargo deb -p gdrive-ui --no-build`.
`make install` builds the packages and prints the APT install command;
`make uninstall` prints the removal command. Neither invokes APT itself.

After building the packages, run this from the repository root:

```sh
sudo apt install ./target/debian/*.deb
```

The `./` prefix tells APT to install local package files rather than search
for package names.

## Running

### OAuth client configuration

Sign-in and token refresh require a Google Cloud OAuth client of type
**Desktop app**, in a project with the Google Drive API enabled. The app
requests full Drive access (`https://www.googleapis.com/auth/drive`).

Set **both** `GDRIVE_CLIENT_ID` and `GDRIVE_CLIENT_SECRET` in the daemon's
runtime environment, unless the build already includes a default client.
A maintainer/packager can embed defaults using
`GDRIVE_BUILD_CLIENT_ID` and `GDRIVE_BUILD_CLIENT_SECRET` at compilation
time, before `cargo build`, `make deb`, or a rebuilding `cargo deb` invocation.
With `--no-build`, credentials must already be in the compiled binary.
Both runtime values together override the embedded pair; a partial runtime
pair does not override either embedded value.

This uses the installed-application OAuth pattern with PKCE; an embedded
client secret is not confidential. Embedded credentials remove per-user
OAuth client setup, **not** the need to sign in and consent in a browser.
See [`gdrive-sync`'s auth module](crates/gdrive-sync/src/auth.rs).

For the packaged daemon, shell exports normally do not reach D-Bus/systemd
activation. Put runtime overrides in `~/.config/gdrived/env` and restart
`gdrived.service`; see
[`gdrived`'s README](crates/gdrived/README.md#packaged-systemdd-bus-activated-daemon).

### From source

Run these commands in separate terminals in your desktop session:

1. Start the daemon with any runtime OAuth credentials exported in its terminal:
   ```sh
   cargo run -p gdrived
   ```
   It loads `~/.config/gdrive-client/config.toml` if present, otherwise uses
   an empty in-memory configuration. It restores cached tokens and serves
   `org.gclient.GDrive1` on the session bus even when signed out.
   Startup does **not** launch interactive authentication.
2. Start the UI:
   ```sh
   cargo run -p gdrive-ui
   ```
3. Click **Sign in** and complete browser consent. The daemon opens the
   browser via `xdg-open` and receives the redirect on `127.0.0.1:8734`.
   Cached sessions refresh automatically while the grant remains valid;
   revoked grants require signing out and in again.
4. Add a sync folder with a local path and a Drive folder selected using
   **Browse**, entered by id, or left empty for "My Drive" root. New folders
   are enabled immediately. Configuration is saved when folders are changed.

### Installed packages

Launch **g-client Google Drive Sync** from the desktop application menu or
run `gdrive-ui`.
On a systemd desktop session, its D-Bus calls activate `gdrived` on demand;
there is no need to run a second daemon manually. Then sign in and add
folders as above. Closing the window hides it to the tray; use the tray's
**Quit** action to exit the UI. The daemon continues running independently.

You can also drive `gdrived` directly without the UI, e.g.:

```sh
busctl --user call org.gclient.GDrive1 /org/gclient/GDrive1 org.gclient.GDrive1 ListSyncFolders
```

## Data locations

Following the XDG base directory spec (see
[`gdrive-common::paths`](crates/gdrive-common/src/paths.rs)), the default
locations are:

- Config: `~/.config/gdrive-client/config.toml`
- Sync state DB: `~/.local/share/gdrive-client/sync-state.sqlite3`
- Cached OAuth token: `~/.local/share/gdrive-client/oauth-token.json`

`XDG_CONFIG_HOME` and `XDG_DATA_HOME` override the config and data base
directories respectively. The packaged systemd unit's optional OAuth
environment file is separately fixed at `~/.config/gdrived/env`; it does
not follow `XDG_CONFIG_HOME`.

## Repository conventions

See [`AGENTS.md`](AGENTS.md) for contributor/agent guidance, including the
documentation-editing convention and how to verify the Qt/QML UI visually.

## Alternatives

If `g-client` doesn't fit your needs, there are other solid ways to work
with Google Drive on Linux:

- **[rclone](https://rclone.org/)** — a command-line tool supporting Drive
  and many other storage backends, with sync, mount, and encryption options.
- **[Insync](https://www.insynchq.com/)** — a commercial desktop sync client
  with Linux support.
- **[odrive](https://www.odrive.com/)** — a third-party cloud sync tool;
  check its current platform support and plan requirements.
- **The Google Drive web app** — no installation at all, works anywhere,
  and always reflects Google's latest features first.

`g-client` aims to be a small, transparent, native alternative for people
who want a minimal tool they can read, build, and run themselves, subject
to the limitations above.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Dependency licenses are checked with [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny)
(`cargo deny check`, config in [`deny.toml`](deny.toml)), and our own source
is scanned for embedded license/copyright text with
[`scripts/scancode-scan.sh`](scripts/scancode-scan.sh) (ScanCode Toolkit).
Both run in CI on pushes to `main` and pull requests — see
[`AGENTS.md`](AGENTS.md#license-compliance-tooling).

ScanCode's tested compatibility versions are pinned in
[`scripts/scancode-requirements.txt`](scripts/scancode-requirements.txt).
Plugincode 32 uses class-marked hooks that Pluggy 1.7 no longer discovers,
so this scanner uses Pluggy 1.6. The bootstrap checks these requirements on
every invocation, repairing incompatible existing `.venv-scancode`
environments as well as setting up fresh ones. Running the scan script can
install or change packages inside that venv; agents must ask the user to
perform any needed local installation rather than doing it themselves.
The bootstrap's offline regression tests use a fake installer and scanner:
`./scripts/test-scancode-bootstrap.py`.
The report checker fails on scan errors and unexpected source notices. Its
path-specific baseline in `.github/scancode-policy.json` recognizes only the project's own license mentions and
the two Debian package manifests' license-file/copyright metadata; it does not
globally exempt source files or third-party copyrights.
