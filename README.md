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
(or "My Drive" root). `gdrived` keeps enabled folders synchronised
continuously; `gdrive-ui` is just a control panel for `gdrived` and holds no
sync state itself.

## Building

Requires a recent stable Rust toolchain (edition 2021, `rust-version = "1.85"`
in the workspace `Cargo.toml`).

```sh
cargo build --workspace
cargo test --workspace
```

`gdrive-ui` additionally requires Qt6 + QML modules to be installed on the
system (see [`scripts/install-qt6.sh`](scripts/install-qt6.sh) for the
Debian/Ubuntu package list). Run that script directly on your development
machine, not inside a restricted sandbox.

## Running

1. Set `GDRIVE_CLIENT_ID` and `GDRIVE_CLIENT_SECRET` (a Google Cloud OAuth
   client) in the environment so `gdrived` can run the interactive OAuth
   flow on first use — see [`gdrive-sync`'s auth module](crates/gdrive-sync/src/auth.rs).
2. Start the daemon:
   ```sh
   cargo run -p gdrived
   ```
   It loads (or creates) `~/.config/gdrive-client/config.toml`, tries to
   authenticate, and serves the `org.gclient.GDrive1` interface on the
   session D-Bus bus regardless of whether authentication succeeded, so the
   UI can always start.
3. Start the UI:
   ```sh
   cargo run -p gdrive-ui
   ```
   Use it to add a sync folder (local path + optional Drive folder id),
   enable it, and `gdrived` will start syncing it.

You can also drive `gdrived` directly without the UI, e.g.:

```sh
busctl --user call org.gclient.GDrive1 /org/gclient/GDrive1 org.gclient.GDrive1 ListSyncFolders
```

## Data locations

Following the XDG base directory spec (see
[`gdrive-common::paths`](crates/gdrive-common/src/paths.rs)):

- Config: `~/.config/gdrive-client/config.toml`
- Sync state DB + cached OAuth token: `~/.local/share/gdrive-client/`

## Repository conventions

See [`AGENTS.md`](AGENTS.md) for contributor/agent guidance, including the
change-logging convention and how to verify the Qt/QML UI visually.

## Alternatives

If `g-client` doesn't fit your needs, there are other solid ways to work
with Google Drive on Linux:

- **[rclone](https://rclone.org/)** — a mature, extremely versatile
  command-line tool supporting dozens of cloud storage backends (not just
  Drive), with powerful sync/mount/encryption options for advanced use
  cases.
- **[Insync](https://www.insynchq.com/)** — a polished, actively maintained
  commercial GUI client with broad desktop integration (file manager
  overlays, selective sync, multiple accounts) and official Linux support.
- **[odrive](https://www.odrive.com/)** — a commercial "sync on demand"
  client that keeps a lightweight local placeholder for cloud files,
  useful when your Drive is much larger than your local disk.
- **The Google Drive web app** — no installation at all, works anywhere,
  and always reflects Google's latest features first.

Each of these has years of polish behind it; `g-client` simply aims to be a
small, transparent, native alternative for people who want a minimal tool
they can read, build, and run themselves.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Dependency licenses are checked with [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny)
(`cargo deny check`, config in [`deny.toml`](deny.toml)), and our own source
is scanned for embedded license/copyright text with
[`scripts/scancode-scan.sh`](scripts/scancode-scan.sh) (ScanCode Toolkit).
Both run in CI on every push/PR — see [`AGENTS.md`](AGENTS.md#license-compliance-tooling).
