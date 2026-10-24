---
name: gdrive-ui-testing
description: Inspect and manipulate an owned gdrive-ui Qt Quick window through bounded, PID-scoped AT-SPI actions, including Wayland where accessibility is exposed, and run the skill's own offscreen Qt tests.
---

# Qt UI testing

Use this skill for semantic Qt window inspection, Add/Cancel smoke tests, Drive
folder navigation, and display-independent Qt test examples. This skill includes
its own helpers, unit tests, and QML fixture; it does not require application
changes or regression fixtures from another branch. A particular application
control still needs to expose functioning accessibility actions.

## Dedicated entry point and narrow approval

Run **only the dedicated executable**, from the repository root:

```sh
./.github/skills/gdrive-ui-testing/gdrive-ui-testing --help
```

Do not run `python`, `python3`, `python -c`, or an inline Python heredoc for this
workflow. The executable resolves and runs only its bundled implementations using
the system interpreter internally. Its `test` and `qml-test` commands cannot be
redirected to arbitrary test code. Do not add broad `allowed-tools: shell`
frontmatter or ask the user to approve Python generally.

When the CLI requests permission, ask the user to allow this specific executable
for subsequent invocations. For a new CLI session, an explicit scoped rule is:

```sh
copilot --allow-tool='shell(./.github/skills/gdrive-ui-testing/gdrive-ui-testing:*)'
```

Keep using the literal repository-relative executable path in tool commands so
the narrow rule matches. This permission covers the trusted script and its
bundled implementation, not arbitrary interpreter commands. Never grant
`shell(python3:*)`, `shell(python:*)`, or `shell(*)` for this skill.

## Safety and prerequisites

- Do not read/source the repository-root `.env`, install anything, invoke
  privileged commands, or use the shell `timeout` command.
- Target only an exact PID of an instance you launched and own. Never select
  another window by title, kill user instances, or stop their daemon.
- Use the graphical user's existing D-Bus session and runtime directory.
  Do not force X11 or invent a replacement bus for a real-window smoke test.
  On Wayland, the Qt bridge and desktop must expose the app through AT-SPI.
- Live testing is read-only: open dialogs, browse, and cancel. Do not sign
  in/out, submit Add, change sync settings, or create/delete files or folders.
  Starting the app may activate `gdrived`; leave that daemon running.
- Save logs, trees, and screenshots in the current agent session's artifact
  directory, not the repository. Trees can contain account/folder names.
- Use finite execution-tool budgets as well as the helper's own deadlines.
  A timed-out action may have happened: inspect before retrying.

The helpers require `/usr/bin/python3`, system GI bindings, and Atspi 2.0, but
no pip packages. The offscreen fixture requires Qt 6 Quick Controls and QtTest.
If something is missing, report it and give the user the relevant command;
**never install it yourself**. Debian/Ubuntu examples:

```sh
sudo apt install python3-gi gir1.2-atspi-2.0 at-spi2-core
sudo apt install qt6-declarative-dev-tools qml6-module-qttest qml6-module-qtquick-controls
```

Only suggest packages actually missing. For a missing Qt 6 accessibility bridge,
check the distro's Qt GUI package; do not recommend the obsolete Qt 5 qt-at-spi.

## Discover capabilities and launch an owned app

```sh
./.github/skills/gdrive-ui-testing/gdrive-ui-testing --timeout 8 check
```

A no-PID check proves desktop access, not exposure of the target app. Build
`gdrive-ui` with the repository's normal build instructions if necessary, without
installing anything or touching the root `.env`.

Launch an owned instance with accessibility enabled before startup, using the
execution tool's attached background facility and a session-local log:

```sh
QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 ./target/debug/gdrive-ui > /absolute/session/files/gdrive-ui.log 2>&1
```

Record the actual application PID from process/tool metadata, not `pgrep` or
title matching. Verify it is running and exposed; substitute that PID below:

```sh
./.github/skills/gdrive-ui-testing/gdrive-ui-testing --pid 12345 --timeout 12 check
./.github/skills/gdrive-ui-testing/gdrive-ui-testing --pid 12345 tree
```

No accessible application is a blocker, not permission to use another process,
coordinates, clipboard injection, or an X11-only fallback.

## Inspect, act, and assert the result

Read the tree first. Names and roles are exact and case-sensitive; names may
differ by Qt version, localization, or application branch. Repeat `--name` for
explicit observed variants. `find` reports actions and EditableText support.

```sh
./.github/skills/gdrive-ui-testing/gdrive-ui-testing --pid 12345 find --name 'Add folder'
./.github/skills/gdrive-ui-testing/gdrive-ui-testing --pid 12345 press --name 'Add folder'
./.github/skills/gdrive-ui-testing/gdrive-ui-testing --pid 12345 tree
```

On older branches the name can be `Add folder…` rather than `Add folder`.
Use observed accessible names, not QML object IDs or assumed visual labels.
The bounded smoke supports the common Add name variants:

```sh
./.github/skills/gdrive-ui-testing/gdrive-ui-testing smoke --pid 12345 --timeout 60
```

This bundled scenario refuses existing dialogs, opens Add, waits for Cancel,
cancels, and verifies the dialog disappeared. It never submits settings and
does not depend on the Drive API, a live account, or app-specific test fixtures.
It can still be blocked by a missing or broken application accessibility action.

Where the actual tree exposes the following names, Drive navigation uses:

```sh
./.github/skills/gdrive-ui-testing/gdrive-ui-testing --pid 12345 press --name 'Browse Drive folders'
./.github/skills/gdrive-ui-testing/gdrive-ui-testing --pid 12345 --timeout 35 wait --name 'Retry' --wait-seconds 33
./.github/skills/gdrive-ui-testing/gdrive-ui-testing --pid 12345 press --name 'Retry'
./.github/skills/gdrive-ui-testing/gdrive-ui-testing --pid 12345 press --name 'Cancel' --ancestor 'Select Drive folder'
```

This is an example error-state sequence, not a promise that every browse fails
or every branch has Retry. Assert the observed loading, success, or error state.
After Cancel, inspect the tree to prove that dialog disappeared, then cancel
the outer Add dialog. Selecting a folder only changes the pending selection;
never confirm the outer Add dialog.

Ambiguous nodes fail closed. Use an observed `--role` and/or `--ancestor` to
disambiguate, especially nested Cancel buttons. For another advertised action:

```sh
./.github/skills/gdrive-ui-testing/gdrive-ui-testing --pid 12345 invoke --name 'Cancel' --ancestor 'Add sync folder' --action 'Press'
```

**Action acceptance is not verified UI success.** Always wait for or inspect
the semantic postcondition before claiming an interaction passed.

### Qt 6.4 accessibility limitations

Quick/Fusion buttons can advertise Press without emitting `clicked`. A provider
may need explicit `Accessible.onPressAction` handling that respects `enabled`.
Report a missing transition as a provider blocker; do not modify the production
app just to force a smoke test through. The bundled QML fixture demonstrates the
handling separately. Named dialog containers help expose/disambiguate controls.

Qt can advertise duplicate Press names. The helper accepts equivalent duplicates,
but distinct Press/Click candidates require explicit `invoke`; node ambiguity
is never ignored.

Text fields may have a `text` role but no EditableText interface. Where `find`
reports `"editable": true`, unsaved field editing can use:

```sh
./.github/skills/gdrive-ui-testing/gdrive-ui-testing --pid 12345 edit --name 'Drive folder id' --role text --text 'unsaved test value'
```

The helper never uses clipboard/key injection. If EditableText is absent,
report that live text entry is unsupported. Use Qt Quick Test's `keyClick` or
`keySequence` for native input tests instead; QML TestCase has no `keyClicks`.
Never put secrets in arguments, and cancel edited dialogs without submitting.

## Self-contained validation

Run the bundled Python unit suite and standalone Qt fixture through the same
dedicated executable:

```sh
./.github/skills/gdrive-ui-testing/gdrive-ui-testing test
./.github/skills/gdrive-ui-testing/gdrive-ui-testing qml-test
```

The unit suite needs no desktop, network, daemon, or GI import. It covers PID
isolation, selectors, ambiguity, cache invalidation, actions, edits, deadlines,
and the executable's fixed dispatch. The Qt fixture uses the Qt 6 runner
offscreen with software rendering; it checks semantic Press, disabled buttons,
and native text entry without relying on any application QML module or fixture.

These are **skill/toolkit tests, not regression coverage for the application's
Drive browser**. Run any application tests separately using that branch's
documented commands. Do not claim a fixed app, successful Drive listing, or live
AT-SPI interaction from an offscreen pass alone.

## Budgets, screenshots, and cleanup

The helper sets bounded AT-SPI startup/call timeouts (1500ms by default).
`--timeout` bounds the complete disposable worker, including GI discovery.
`wait` also has `--wait-seconds` and `--poll-seconds`; leave time for IPC inside
the overall budget. Polls clear AT-SPI caches before reading names, states and
children rather than assuming a sleeping client dispatches GLib change events.
`--max-nodes`/`--max-depth` fail instead of returning incomplete ambiguous trees.
Actions require SHOWING, ENABLED and SENSITIVE; hidden inspection is not action
authorization. Errors are JSON with exit 2; the hard worker deadline exits 124.

If taking a screenshot immediately after launch, use the documented active-window
capture without raising/focusing through window-manager tools:

```sh
sleep 5 && spectacle -a -b -n -o /absolute/session/files/gdrive-ui.png
```

Do not share a capture of an unrelated window as UI evidence. Screenshots are
evidence, not the interaction mechanism.

Stop only the exact owned UI PID or its attached execution-tool session; closing
the window can merely hide it to the tray. Do not stop `gdrived`. Report actual
selectors, postconditions, artifacts, test counts, and blockers, distinguishing
unit, offscreen, and real-window coverage.
