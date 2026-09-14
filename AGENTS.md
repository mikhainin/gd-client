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
