# gdrive-ui

Qt6/QML desktop control panel for `gdrived`, built with
[cxx-qt](https://github.com/KDAB/cxx-qt) (no CMake required).

- `cxxqt_object.rs` — the `SyncManager` `QObject` exposed to QML: lists,
  adds, removes, and enables/disables sync folders by calling `gdrived`'s
  D-Bus interface (`gdrive_common::dbus_api::GDrive1`), exposing a JSON
  `foldersJson` property for the QML `ListView` model, plus a `darkMode`
  property. All daemon requests and the startup color-scheme lookup run
  away from the GUI thread and deliver results through cxx-qt's Qt queue.
- `dbus_client.rs` — bounded asynchronous requests using the shared
  `GDrive1Proxy`, executed on a two-thread Tokio runtime. Request generations
  and abort handles reject obsolete completions and cancel abandoned work.
- `qml/main.qml` — the UI itself: a folder list with add/remove/enable
  controls, deliberately avoiding `QtQuick.Layouts` (not always packaged
  alongside base QtQuick Controls) in favour of plain `Row`/`Column`.
- `qml/DriveFolderDialog.qml` — asynchronous Drive folder browser, also
  exercised directly by the Qt Quick tests.
- `qml/DriveFolderSelection.qml` and `qml/DriveFolderPath.qml` — pending
  ID/path selection and the shared readable path display.
- `qml/TrayPresentation.qml` — tray icon and tooltip selection from explicit
  connection, authentication, and engine-status properties.
- `build.rs` — builds the QML module (`org.gclient.gdrive_ui`) via
  `cxx-qt-build`.

## Prerequisites

Qt6 + the QML modules used by QtQuick Controls' Fusion style must be
installed (see [`scripts/install-qt6.sh`](../../scripts/install-qt6.sh) for
the Debian/Ubuntu package list).
SVG icons require the Qt SVG runtime plugin (`libqt6svg6` on Debian/Ubuntu),
including when running directly from Cargo. The Debian package declares this
dependency explicitly.

## Running

```sh
cargo run -p gdrive-ui
```

Requires `gdrived` to be running for the "Connected to gdrived" status and
folder list to populate; the UI itself holds no sync state.

## Drive folder paths

The Add dialog displays `My Drive/Parent/Selected folder` after browsing, with
`My Drive` for root. The opaque ID stays separate and is used for syncing.
**Advanced: enter Drive folder ID** exposes manual entry; changing that ID
invalidates the previously selected path. Canceling the browser preserves the
pending selection, and adding a folder resets it to root.

Configured folders show the saved remote path alongside their display name
and local destination. Long paths are elided, with full text available through
accessibility and a window-width-bounded, wrapped hover tooltip (including
long unbroken folder names). Legacy and manually entered non-root folders without
a saved path show **Drive path unavailable; reselect folder**, not the ID.
Select through Browse when adding a folder to save its path; this change does
not introduce editing of existing entries or automatic path lookups.
Paths are selection-time snapshots, so remote moves/renames do not update them.

The added path argument in `AddSyncFolder` and field in `ListSyncFolders`
change their D-Bus signatures. Upgrade the UI and daemon together and restart
the daemon; mixed versions are unsupported and surface D-Bus errors.

## Application identity and tray status

The original violet folder icon combines a path-drawn lowercase **g** with
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
outstanding errors. Existing Show/Hide, Quit, and hide-on-close actions are
unchanged.

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
connection, service activation, and the method reply. The daemon bounds its
listing to 27 seconds and its underlying paginated Drive request to 25
seconds. A loading indicator appears immediately; Cancel remains usable.
Failures appear in the dialog with Retry, not as an empty folder listing.
Changing folders or closing the dialog aborts the pending UI request and
invalidates any completion already queued to Qt.

Other daemon requests and the portal preference lookup have a **5-second
overall deadline**. Polls are coalesced while a refresh is pending, and
mutation controls are disabled during a mutation. Starting a mutation
invalidates an older refresh; its completion triggers a fresh snapshot.
Action errors remain visible across successful polling refreshes until the
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
a signal; periodic polling remains a fallback for older daemons and sync status.
Browsing and mutations still execute independently, not through a serial queue.
The native `kdialog` local-folder picker is a separate, unchanged synchronous
path; these guarantees apply to D-Bus requests.

## UI testing

The repository skill
[`gdrive-ui-testing`](../../.github/skills/gdrive-ui-testing/SKILL.md)
provides semantic real-window interaction through AT-SPI accessibility,
without X11 input automation. Enable Qt accessibility when launching an
owned test instance with `QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1`. Never change
real sync folders as part of a smoke test.

The Qt Quick tests require `qml6-module-qttest` on Debian/Ubuntu. If it is
missing, ask the user to run `sudo apt install qml6-module-qttest`; agents
must not install it themselves.

```sh
cargo test -p gdrive-ui
cargo run -p gdrive-ui --example verify-icons
dbus-run-session -- env GDRIVE_UI_TEST_BUS=1 cargo test -p gdrive-ui -- --ignored --test-threads=1
QT_QPA_PLATFORM=offscreen QT_QUICK_CONTROLS_STYLE=Fusion \
  /usr/lib/qt6/bin/qmltestrunner -input crates/gdrive-ui/tests/qml
```

Run these from the workspace root. The ignored Rust test deliberately uses
a private D-Bus session to test a stalled daemon, authentication signals, daemon
ownership changes, subscription cleanup, and portal color-scheme variants
(including invalid types) without touching the user's daemon or desktop portal.
The Qt tests use a controllable backend double for loading, errors,
retry, cancellation, late replies, breadcrumbs, and root/nested selection;
they also measure GUI heartbeat/cancel responsiveness while work is pending.
The production path/selection controls are tested for readable nested/root
paths, legacy/manual unknown paths, native manual input invalidation,
reselection/reset, cancellation, and literal folder names. Private-bus tests
verify ID/path transport; daemon/configuration tests cover persistence and
loading old configuration without a path.
Switch tests cover native and accessible input, failed updates, and subsequent
backend changes without losing the checked-state binding.
Tray tests cover state precedence, transitions, unavailable status, and decoding
every selected SVG. The standalone `verify-icons` Qt probe renders all embedded
variants at
16/22/24/32/48/128 pixels and checks application/new-window icon propagation.
Set `GDRIVE_ICON_PREVIEWS` to an existing absolute artifact directory to save
light/dark-background PNG previews during that probe. Qt runs on its process's
main thread, not on a Rust unit-test worker; it does not load the UI or contact
the daemon. Missing SVG support fails
the rendering tests explicitly. Private-bus tests also cover status signals,
lost-signal polling recovery, stale revisions, daemon replacement, and timeout
recovery; run ignored UI tests serially because they own the same service name.

`tests/live_browse.py` drives the actual debug binary using the skill,
against `tests/mock_daemon.py` on a private D-Bus session. It tests the
production 30-second deadline, cancellation, errors/retry, nested selection,
late replies, and authentication signals while polling is stalled, without
submitting a sync folder or authenticating a real account. It uses the skill's
dedicated executable. With the desktop accessibility bus address available, run:

```sh
dbus-run-session -- env GDRIVE_UI_TEST_BUS=1 \
  crates/gdrive-ui/tests/live_browse.py --artifacts /absolute/session/artifacts
```

Logs go to the supplied artifact directory. The fixture also mocks the portal
to avoid activating real desktop services on its private bus. Cancellation
measurements wait for the scoped Drive-dialog Cancel control to disappear,
not for a control behind the modal dialog to remain visible.

`AccessibleButton.qml` explicitly connects accessible Press to the enabled
button's click action, including dialog footer buttons. This is necessary
on Qt 6.4, which can advertise Press without activating a Quick button.
Named accessible containers disambiguate nested dialog actions. Qt 6.4 may
not expose EditableText for Quick text fields; the skill reports this
limitation explicitly instead of pretending that text entry succeeded.

## Dark mode

The UI follows the desktop's dark/light preference. At startup, `main.rs`
sets the Qt Quick Controls fallback style to "Fusion" (which draws every
control from the QML `palette` property, unlike the default "Basic" style),
and the asynchronous startup request queries the freedesktop desktop
portal's `org.freedesktop.appearance` `color-scheme` setting over D-Bus
(supported by `xdg-desktop-portal-kde`/`-gnome`/`-gtk` on effectively every
modern desktop, so this doesn't depend on a Qt6-specific platform theme
plugin being installed). `qml/main.qml` applies an approximate Breeze Dark
`Palette` to the window when `SyncManager.darkMode` is true. The preference
is only read once at startup (not live-updated if changed while running).
The window starts with the default palette and updates when the bounded
portal request completes; an unavailable portal logs a warning and leaves
the light fallback in place.
