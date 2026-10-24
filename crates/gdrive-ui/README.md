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
- `build.rs` — builds the QML module (`org.gclient.gdrive_ui`) via
  `cxx-qt-build`.

## Prerequisites

Qt6 + the QML modules used by QtQuick Controls' Fusion style must be
installed (see [`scripts/install-qt6.sh`](../../scripts/install-qt6.sh) for
the Debian/Ubuntu package list).

## Running

```sh
cargo run -p gdrive-ui
```

Requires `gdrived` to be running for the "Connected to gdrived" status and
folder list to populate; the UI itself holds no sync state.

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
dbus-run-session -- env GDRIVE_UI_TEST_BUS=1 cargo test -p gdrive-ui -- --ignored
QT_QPA_PLATFORM=offscreen QT_QUICK_CONTROLS_STYLE=Fusion \
  /usr/lib/qt6/bin/qmltestrunner -input crates/gdrive-ui/tests/qml
```

Run these from the workspace root. The ignored Rust test deliberately uses
a private D-Bus session to test a stalled daemon, authentication signals, daemon
ownership changes, and subscription cleanup without touching the user's daemon.
The Qt tests use a controllable backend double for loading, errors,
retry, cancellation, late replies, breadcrumbs, and root/nested selection;
they also measure GUI heartbeat/cancel responsiveness while work is pending.

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
to avoid activating real desktop services on its private bus.

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
