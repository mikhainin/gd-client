# AGENTS.md

Guidance for AI agents (and human contributors) working in this repository.

## Project

This is a Rust project.

## Documentation

- There is no changelog. Do not append change-log-style, dated entries anywhere.
- Every meaningful change must instead be reflected by editing the relevant existing section in this file, or in the corresponding documentation file for the affected crate/module (e.g. its own README.md), so the docs always describe only the current state of the project.
- "Meaningful" means anything that affects behavior, architecture, dependencies, configuration, or public interfaces — not typo fixes or formatting-only edits.
- When a change belongs to a specific documented area (e.g. a crate's own README), edit it there; add or update a dedicated section in this file only for guidance that applies across the whole repository.

## Dependency management

- When adding a new crate dependency, ALWAYS check the latest available version first via the crates.io REST API, e.g.:
  `curl -s https://crates.io/api/v1/crates/<crate_name> | jq '.crate.max_stable_version'`
  Do not rely on memory or guesses for version numbers.
- Never downgrade an existing dependency in `Cargo.toml`.
- To update an existing dependency, only upgrade it. If a downgrade seems necessary, stop and ask the user what to do before proceeding.

## Running commands

- Do not prefix commands (e.g. `cargo run`, daemon binaries) with the `timeout` shell command. Use the tool's own async/background/long-running command support instead to observe and then stop a process.
- Never run `sudo apt` (or any other privileged package installation command) yourself, even with explicit user approval of *what* to install. Always give the user the exact command and ask them to run it themselves.

## Network and IPC calls must always have a timeout

- Every network call (HTTP requests, e.g. via `reqwest`) and every IPC call (e.g. D-Bus calls, whether via `zbus` async or `zbus::blocking`) MUST have an explicit, bounded timeout. Never rely on a client/proxy's default settings without verifying a timeout is actually set - many defaults (e.g. a bare `reqwest::Client::new()`, or a default `zbus` proxy/connection) have NO timeout and can hang indefinitely.
- This applies transitively: if a UI-thread call (e.g. a `#[qinvokable]` on a Qt/QML `QObject`) blocks on a D-Bus call, which in turn blocks on an HTTP call, every link in that chain needs its own timeout - a timeout deep in the chain does not make the calls above it safe on its own, since blocking UI-thread calls should also not be left to block for the full duration of a slow-but-not-infinite timeout (prefer moving such calls off the UI thread as well).
- When adding a new network or IPC call, always set a timeout and verify the failure path (what happens when it fires) rather than assuming the underlying library already handles it.

## Verifying the Qt/QML UI visually

- To screenshot the running `gdrive-ui` window, do not try to raise/focus it via `wmctrl`/`xdotool`/KWin scripting - just run `sleep 5 && spectacle -a -b -n -o /path/to/file.png` right after launching it, which captures the active window directly (`-a`), in the background (`-b`), with no capture-start sound (`-n`).

## License compliance tooling

- `deny.toml` configures `cargo-deny` (`cargo install cargo-deny`) to check dependency licenses (`cargo deny check licenses`), security advisories, banned/duplicate crates, and source registries (`cargo deny check`). The `[licenses.allow]` list must stay in sync with the workspace's own `license = "MIT OR Apache-2.0"` - only add an SPDX id there once you've confirmed it's compatible.
- `scripts/scancode-scan.sh` runs ScanCode Toolkit (via a `uv`-managed venv in `.venv-scancode/`, gitignored) over our own source (not dependencies) to catch any embedded license text/copyright notices that might indicate copy-pasted code from an incompatibly-licensed project. Requires `uv` (`curl -LsSf https://astral.sh/uv/install.sh | sh`); the script bootstraps everything else itself. Output goes to `.scancode/scancode-report.json` (also gitignored).
- Both checks run in CI via `.github/workflows/license-check.yml` on every push/PR.

