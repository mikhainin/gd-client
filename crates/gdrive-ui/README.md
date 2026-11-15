# gdrive-ui

Qt6/QML desktop control panel for `gdrived`, built with
[cxx-qt](https://github.com/KDAB/cxx-qt) (no CMake required).

- `cxxqt_object.rs` — QML's `SyncManager` `QObject`: folder JSON, authentication,
  tray status, preferences, and action results. Daemon/portal requests run off
  the GUI thread and return through cxx-qt's Qt queue.
- `dbus_client.rs` — bounded asynchronous requests using the shared
  `GDrive1Proxy`, executed on a two-thread Tokio runtime. Request generations
  and abort handles reject obsolete completions and cancel abandoned work.
- `qml/main.qml` — window, tray, authentication, and folder controls; uses
  `Row`/`Column` rather than requiring `QtQuick.Layouts`.
- `qml/DriveFolderDialog.qml` — asynchronous Drive folder browser.
- `qml/DriveFolderSelection.qml` and `qml/DriveFolderPath.qml` — pending
  ID/path selection and the shared readable path display.
- `qml/TrayPresentation.qml` — tray icon and tooltip selection from explicit
  connection, authentication, and engine-status properties.
- `application.rs`/`application.h` — the Rust/C++ `QApplication` factory;
  `app_icon.rs`/`app_icon.h` set the embedded application icon.
- `build.rs` — builds the QML module (`org.gclient.gdrive_ui`) via
  `cxx-qt-build`.

## Prerequisites

Qt6 + the QML modules used by QtQuick Controls' Fusion style must be
installed (see [`scripts/install-qt6.sh`](../../scripts/install-qt6.sh) for
the Debian/Ubuntu package list).
SVG icons require the Qt SVG runtime plugin (`libqt6svg6` on Debian/Ubuntu),
including when running directly from Cargo. The Debian package declares this
dependency explicitly.
The application also links Qt Widgets and starts a `QApplication`, not just a
`QGuiApplication`: `Qt.labs.platform` can fall back to widget-backed tray menus
when the desktop does not supply a native menu implementation.

## Running

```sh
cargo run -p gdrive-ui
```

From source, run `gdrived` separately in the same desktop session; packaged
installs activate it over D-Bus. See the [daemon README](../gdrived/README.md)
for OAuth setup, activation, configuration locations, and diagnostics, and the
[workspace build instructions](../../README.md#building) for packaging commands.
The daemon owns persisted configuration and synchronization; the UI keeps
only presentation and pending-request state.

**Sign in…** asks the daemon to start browser consent; startup never starts
sign-in itself. The five-second control deadline covers starting that flow,
not completing consent. Authentication means a cached token exists, not that
Google has just validated it. Asynchronous sign-in failures are currently
reported in daemon logs, not delivered as a UI error. **Sign out** forgets the
token and stops workers without removing configured folders.

**Add folder…** accepts a display name, Drive selection, local path, and optional
owner user/group, and always submits the new folder as enabled. Local Browse
prefers `kdialog`, otherwise an in-app directory browser supports navigation
and creating a subfolder. Local paths can also be typed directly. Accepting
Add clears the name, Drive selection, and local path immediately, even if the
daemon later rejects the request; owner fields remain. Canceling Add preserves
its pending fields. There is no edit operation for existing entries.
**Remove** stops/removes the configuration without deleting its local or remote
files and has no confirmation dialog.

Folder-row labels use worker lifetime: **syncing** means a worker is running
(possibly idle or waiting to retry), **starting…** means enabled with no
running worker, and **paused** means disabled. Use the tray for actual activity
and engine errors. Closing the window hides it; the tray toggles Show/Hide and
**Quit** exits only the UI, leaving the daemon running.

## Drive folder paths

The Add dialog displays `My Drive/Parent/Selected folder` after browsing, with
`My Drive` for root. The opaque ID stays separate and is used for syncing.
**Advanced: enter Drive folder ID** exposes manual entry; changing that ID
invalidates the previously selected path. Each Browse opens at My Drive;
canceling it preserves the pending selection.

Configured folders show the saved remote path alongside their display name
and local destination. Long paths are elided, with full text available through
accessibility and a window-width-bounded, wrapped hover tooltip (including
long unbroken folder names). Legacy and manually entered non-root folders without
a saved path show **Drive path unavailable; reselect folder**, not the ID.
Select through Browse when adding a folder to save its path; existing entries
have no path-reselection control or automatic path lookup.
Paths are selection-time snapshots, so remote moves/renames do not update them.

`AddSyncFolder` and `ListSyncFolders` transport both ID and path. Upgrade the UI
and daemon together and restart the daemon: older versions without the path
argument/field have incompatible D-Bus signatures.

## Application identity and tray status

The violet folder icon combines a path-drawn lowercase **g** with
sync arrows, independently of Google's branding. SVGs under `icons/` are embedded
in the binary, so window and tray icons do not depend on an installed theme.
The static identity is installed as
`hicolor/scalable/apps/org.gclient.gdrive-ui.svg`; the desktop entry and Qt
desktop-file identity both use `org.gclient.gdrive-ui`. This also associates
Wayland windows with the launcher. Title-bar icons remain compositor-dependent;
some desktops deliberately do not show them.

Only the tray changes with sync status. Disconnected/signed-out takes precedence
over sync errors, then syncing, then idle. Syncing means initial reconciliation
or actual upload/download/change application, not an idle watcher, empty remote
poll, folder browser, or UI operation. Error and disconnected variants have
distinct shapes, not just different colors. Tooltips distinguish startup,
disconnection, sign-out, and unavailable status, and describe the state and
outstanding errors. See the [sync engine README](../gdrive-sync/README.md) for
activity, retry, and error-clearing semantics.

The UI uses `SyncStatus` snapshots plus `SyncStatusChanged` invalidations,
subscribing before its initial snapshot. Status runs on a separate cancellable
worker from authentication and folder requests, with five-second bounds on
setup and method calls. Snapshots are tied to a unique daemon bus owner and
monotonic revision; reconnect resets revision tracking. Three-second polling
recovers missing signals. Unsupported methods or timeouts explicitly show
**Sync status unavailable** without discarding the folder/authentication state
or pretending the engine is idle. UI operation failures still use the existing
status message; the tray's sync errors come from the engine.

## Responsiveness and errors

Drive browsing has a **30-second overall deadline**, including session-bus
connection, service activation, and the method reply. The
[daemon's shorter browse bounds](../gdrived/README.md) leave headroom for
delivery. A loading indicator appears immediately; Cancel remains usable.
Failures appear in the dialog with Retry, not as an empty folder listing.
Changing folders or closing the dialog aborts the pending UI request and
invalidates any completion already queued to Qt.

Other daemon requests and the portal preference lookup have a **5-second
overall deadline**. Polls are coalesced while a refresh is pending, and
mutation controls are disabled during a mutation. Starting a mutation
invalidates an older refresh; its completion triggers a fresh snapshot.
Mutation errors remain visible across successful polling refreshes until the
next action, rather than being immediately replaced by the folder count.
Folder switches remain bound to the backend state while a change is pending
or fails, including changes requested through accessibility. A successful folder
snapshot remains visible and connected even if its separate authentication read
fails or times out; the previous authentication state is retained and the status
reports that failure. Both reads share the same five-second overall deadline.
`AuthenticationChanged` signals update sign-in/out state immediately and
invalidate older snapshots. Each `SyncManager` owns one cancellable subscription;
daemon ownership changes replace it, and transport failures retry after three
seconds. Subscription setup calls have five-second deadlines. A snapshot is
requested after subscribing so initial state and restarts do not rely on receiving
a signal; periodic polling remains a fallback for authentication.
Browsing and mutations still execute independently, not through a serial queue.
These guarantees apply to D-Bus requests, not local filesystem operations
or the synchronous `kdialog --version` probe and local-folder picker, which
can block the GUI thread and have no explicit timeout.

## UI testing

### Hidden full-application smoke

From the workspace root, with the normal Qt prerequisites and `dbus-daemon`,
`busctl`, and Python 3 available:

```sh
cargo build -p gdrived -p gdrive-ui --features gdrived/test-support,gdrive-ui/test-support
./scripts/smoke-full-app.py --artifacts target/full-app-smoke
```

This optional runner launches the **real daemon and actual UI main/QML** on a
disposable private D-Bus with no activatable services, using Qt offscreen/software.
It inspects visible QML connection, authentication, and folder text, the real
Qt.labs.platform widget-backed menu, and explicitly constructs/triggers a
`QMenu` in the same process to catch accidental `QGuiApplication` regressions.
The stateful loopback Drive checks the synthetic bearer token and exercises
initial download, a watched local modification/upload, an injected HTTP 503
shown through the UI's sync-error state, remote download retry, and idle recovery.
It accepts both legacy replacement Trash and retained-history rename requests;
this is an integration smoke, not a replacement-retention policy assertion.

Children use a clean environment, synthetic nonrefreshable tokens, and unique
config/data/cache/runtime/local roots below the artifact directory, never the
current account or sync folders. `--test-smoke` requires `test-support` and
`GDRIVE_UI_TEST_BUS=1`; normal binaries reject it before startup.
The `test-support` [Drive override](../gdrive-sync/README.md) is absent from
normal builds. **Do not package feature-enabled binaries.** Build ordinary
binaries again without these features before packaging.

Readiness uses bus ownership, atomic snapshots of visible production QML text
and backend properties, sync-completion events, and exact file/fake-server
content, not fixed startup sleeps. The shared stage-wait deadline is 60 seconds
(`--deadline` allows 5–120); HTTP/IPC/subprocess calls and owned-child cleanup
have separate finite bounds. Unexpected requests and early child exits fail
with logs, UI snapshots, request traces, and a stage-specific `result.json`
under the printed run directory. Successful runs remove disposable state but
retain evidence; failed runs retain state for diagnosis. Only owned child
handles are terminated, in reverse order. `--bin-dir` selects a separately
staged build; otherwise `$CARGO_TARGET_DIR/debug` or `target/debug` is used.
This is **offscreen native integration**, not live-window/AT-SPI validation
or a pixel/screenshot comparison. It does not exercise interactive OAuth,
Add/Browse controls, a real tray host, or real Google Drive.

### Component and live-window tests

The [`gdrive-ui-testing` skill](../../.github/skills/gdrive-ui-testing/SKILL.md)
documents owned-window AT-SPI interaction, prerequisites, safety constraints,
and its separate toolkit fixture tests. Those fixtures do not load application
QML or prove live accessibility works. Application component tests are below.

The Qt Quick tests require `qml6-module-qttest` on Debian/Ubuntu. If it is
missing, ask the user to run `sudo apt install qml6-module-qttest`; agents
must not install it themselves.

```sh
cargo test -p gdrive-ui
cargo run -p gdrive-ui --example verify-icons
dbus-run-session -- env GDRIVE_UI_TEST_BUS=1 cargo test -p gdrive-ui -- --ignored --test-threads=1
QT_QPA_PLATFORM=offscreen QT_QUICK_BACKEND=software QT_QUICK_CONTROLS_STYLE=Fusion \
  /usr/lib/qt6/bin/qmltestrunner -input crates/gdrive-ui/tests/qml
```

Run these from the workspace root:

- Ordinary Rust tests cover cancellation/generations, a stalled connection
  deadline, and status aggregation. Ignored tests use private daemon/portal
  doubles for authentication/status signals, ownership changes, subscription
  cleanup, ID/path transport, partial authentication failures, color-scheme
  values/types, polling, stale revisions, and timeout recovery. Run them
  serially because they own the same service name.
- Qt Quick tests load production components with a backend double: loading,
  retry/errors, cancellation/late replies, heartbeat responsiveness, navigation,
  ID/path selection and manual editing, readable/literal/long paths, accessible
  button actions, switch bindings after failed changes, and tray precedence/SVGs.
  The double implements request cancellation; these tests alone do not verify
  the Rust bridge or a real D-Bus deadline.
- `verify-icons` uses the production `QApplication` factory, constructs/triggers
  a widget-backed menu, renders embedded icons at 16/22/24/32/48/128 pixels, and
  checks application/new-window icon propagation. It forces offscreen Qt on the
  process main thread without loading the UI or contacting the daemon. Missing
  SVG support fails explicitly. Set `GDRIVE_ICON_PREVIEWS` to an existing absolute
  directory for light/dark-background PNG previews.

`tests/live_browse.py` drives the actual debug binary using the skill,
against `tests/mock_daemon.py` on a private D-Bus session. It tests the
production 30-second deadline, cancellation, errors/retry, nested selection,
late replies, and authentication signals while polling is stalled, without
submitting a sync folder or authenticating a real account. It uses the skill's
dedicated executable. Unlike the skill's normal existing-session launch,
the session bus is private, but the accessibility bus
is the desktop's. It requires system Python GI/Gio, `dbus-run-session`, `busctl`,
the skill's AT-SPI prerequisites, and a built `target/debug/gdrive-ui`
(it does not honor `CARGO_TARGET_DIR`). Set `AT_SPI_BUS_ADDRESS` to the actual
desktop accessibility bus address before running:

```sh
dbus-run-session -- env GDRIVE_UI_TEST_BUS=1 \
  crates/gdrive-ui/tests/live_browse.py --artifacts /absolute/session/artifacts
```

Logs go to the supplied artifact directory. The fixture also mocks the portal
to avoid activating real desktop services on its private bus. Cancellation
measurements wait for the scoped Drive-dialog Cancel control to disappear,
not for a control behind the modal dialog to remain visible.

`AccessibleButton.qml` connects accessible Press to enabled buttons' click
actions, including dialog footers; named containers disambiguate nested
actions. The skill describes Qt 6.4 Press/EditableText limitations.

## Dark mode

`main.rs` sets the Qt Quick Controls fallback style to **Fusion**, which
supports the QML palette; an explicit style override is still honored.
An asynchronous startup request reads the desktop portal's
`org.freedesktop.appearance` `color-scheme` setting. Unsigned value `1` enables
`SyncManager.darkMode` and the approximate Breeze Dark palette in `main.qml`;
other unsigned values leave the default palette, and invalid types fail the
read. No Qt-specific platform theme plugin is required, but the installed
portal backend must provide the setting. The window uses its default palette
until the request completes; failures log a warning and keep that default.
This is a one-time read, not a live preference subscription.
