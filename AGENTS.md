# AGENTS.md

Guidance for AI agents (and human contributors) working in this repository.

## Project

This is a Rust project.

## Change logging

- Every meaningful change must be recorded either in this file (under "Change Log" below) or in the corresponding documentation file for the affected crate/module (e.g. its own README.md).
- "Meaningful" means anything that affects behavior, architecture, dependencies, configuration, or public interfaces — not typo fixes or formatting-only edits.
- When a change belongs to a specific documented area (e.g. a crate's own README), record it there instead of duplicating it here, and add a one-line pointer here if it's significant.

## Dependency management

- When adding a new crate dependency, ALWAYS check the latest available version first via the crates.io REST API, e.g.:
  `curl -s https://crates.io/api/v1/crates/<crate_name> | jq '.crate.max_stable_version'`
  Do not rely on memory or guesses for version numbers.
- Never downgrade an existing dependency in `Cargo.toml`.
- To update an existing dependency, only upgrade it. If a downgrade seems necessary, stop and ask the user what to do before proceeding.

## Running commands

- Do not prefix commands (e.g. `cargo run`, daemon binaries) with the `timeout` shell command. Use the tool's own async/background/long-running command support instead to observe and then stop a process.

## Verifying the Qt/QML UI visually

- To screenshot the running `gdrive-ui` window, do not try to raise/focus it via `wmctrl`/`xdotool`/KWin scripting - just run `sleep 5 && spectacle -a -b -n -o /path/to/file.png` right after launching it, which captures the active window directly (`-a`), in the background (`-b`), with no capture-start sound (`-n`).

## License compliance tooling

- `deny.toml` configures `cargo-deny` (`cargo install cargo-deny`) to check dependency licenses (`cargo deny check licenses`), security advisories, banned/duplicate crates, and source registries (`cargo deny check`). The `[licenses.allow]` list must stay in sync with the workspace's own `license = "MIT OR Apache-2.0"` - only add an SPDX id there once you've confirmed it's compatible.
- `scripts/scancode-scan.sh` runs ScanCode Toolkit (via a `uv`-managed venv in `.venv-scancode/`, gitignored) over our own source (not dependencies) to catch any embedded license text/copyright notices that might indicate copy-pasted code from an incompatibly-licensed project. Requires `uv` (`curl -LsSf https://astral.sh/uv/install.sh | sh`); the script bootstraps everything else itself. Output goes to `.scancode/scancode-report.json` (also gitignored).
- Both checks run in CI via `.github/workflows/license-check.yml` on every push/PR.

## Change Log

- 2026-09-09: Initialized repository (Rust project) and added AGENTS.md with change-logging and dependency-management rules.
- 2026-09-11: Added `gdrived` (background sync daemon + `org.gclient.GDrive1` D-Bus service), `gdrive-ui` (Qt6/QML front-end talking to it over D-Bus), and a shared `gdrive_common::dbus_api` proxy/interface definition. Reworked `SyncEngine` to support starting/stopping individual sync pairs at runtime (`start_pair`/`stop_pair`/`start_all`/`running_pairs`) instead of only running all pairs for the daemon's lifetime. Pinned `reqwest`'s TLS feature to `rustls` (was `rustls-tls`, invalid for the workspace's reqwest 0.13) and disabled `oauth2`'s bundled `reqwest` (version conflict) in favour of a small adapter in `gdrive-sync::auth` reusing our own `reqwest::Client`. Wrapped `StateDb`'s `rusqlite::Connection` in a `Mutex` so it's `Send`/`Sync` across `tokio::spawn`ed per-pair tasks.
- 2026-09-12: Renamed the "Pair"/"SyncPair" terminology to "Folder"/"SyncFolder" throughout the codebase for clarity, since each entry really represents one synced local↔Drive folder rather than a pairing of two things. This touched the config model (`gdrive_common::config::SyncFolder`, `AppConfig::sync_folders`), the D-Bus contract (`gdrive_common::dbus_api`: `SyncFolderRow`, `ListSyncFolders`/`AddSyncFolder`/`RemoveSyncFolder`/`SetFolderEnabled`), the state DB schema (`sync_folder_state`/`sync_folder_id` columns - safe to rename directly since unreleased), `SyncEngine` (`start_folder`/`stop_folder`/`running_folders`, `SyncEvent::FolderStarted`/etc. with `folder_id` fields), `gdrived`'s D-Bus service impl, and `gdrive-ui`'s `SyncManager` QObject/QML (`foldersJson`, `addFolder`/`removeFolder`/`setFolderEnabled`, UI text "Sync folders"/"Add folder…"). Verified with `cargo build --workspace`, `cargo test --workspace`, and a live `busctl` round-trip against the renamed D-Bus methods.
- 2026-09-12: Added a root `README.md` (workspace overview, build/run instructions, data locations) and a `README.md` per crate (`gdrive-common`, `gdrive-sync`, `gdrived`, `gdrive-ui`) summarizing each crate's modules/responsibilities.
- 2026-09-12: Added `LICENSE-APACHE` (downloaded verbatim from `https://www.apache.org/licenses/LICENSE-2.0.txt`) and `LICENSE-MIT` (downloaded verbatim from the SPDX license list at `https://spdx.org/licenses/MIT.txt`, with the `<year> <copyright holders>` placeholder filled in as `2026 g-client contributors`), matching the `license = "MIT OR Apache-2.0"` already declared in the workspace `Cargo.toml`. Linked both from a new "License" section in `README.md`.
- 2026-09-12: Set up ongoing license-compliance scanning: `deny.toml` for `cargo-deny` (dependency license allow-list plus advisories/bans/sources checks - verified clean against the full workspace dependency tree), and `scripts/scancode-scan.sh` (ScanCode Toolkit, via a `uv`-managed venv) to scan our own source for embedded license/copyright text that could indicate incompatibly-licensed copy-pasted code (verified clean). Both run automatically in a new `.github/workflows/license-check.yml` CI workflow. Installed `uv` for managing Python tooling venvs and added `~/.local/bin` to `PATH` in `~/.bashrc`.
