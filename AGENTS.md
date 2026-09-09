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

## Change Log

- 2026-09-09: Initialized repository (Rust project) and added AGENTS.md with change-logging and dependency-management rules.
