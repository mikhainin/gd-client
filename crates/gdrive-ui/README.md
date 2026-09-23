# gdrive-ui

Qt6/QML desktop control panel for `gdrived`, built with
[cxx-qt](https://github.com/KDAB/cxx-qt) (no CMake required).

- `cxxqt_object.rs` — the `SyncManager` `QObject` exposed to QML: lists,
  adds, removes, and enables/disables sync folders by calling `gdrived`'s
  D-Bus interface (`gdrive_common::dbus_api::GDrive1`), exposing a JSON
  `foldersJson` property for the QML `ListView` model, plus a `darkMode`
  property (see `dbus_client::query_prefers_dark`). Its `#[qinvokable]`s
  never wait for a reply (see "Asynchronous D-Bus" below).
- `dbus_client.rs` — the asynchronous D-Bus client: one background thread
  running a current-thread Tokio runtime that owns a single session bus
  connection and `GDrive1Proxy` (async, auto-generated from the shared
  `GDrive1` trait), fed by a bounded request channel. Also exposes
  `query_prefers_dark()`, which asks the freedesktop desktop portal
  (`org.freedesktop.portal.Desktop`) for the user's dark/light color-scheme
  preference, and `set_authentication_listener()`, which forwards `gdrived`'s
  `AuthenticationChanged` signal.
- `qml/main.qml` — the UI itself: a folder list with add/remove/enable
  controls, deliberately avoiding `QtQuick.Layouts` (not always packaged
  alongside base QtQuick Controls) in favour of plain `Row`/`Column`.
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

## Asynchronous D-Bus

`#[qinvokable]` methods run on the Qt/QML UI thread, so none of them may wait
for a D-Bus round-trip: a stalled session bus or an unresponsive `gdrived`
would otherwise freeze the UI. Instead:

- `dbus_client` owns one long-lived worker thread (never one thread per call)
  with a single `zbus` connection and async proxy on it.
- Invokables submit a request over a bounded channel (16 entries) and return
  immediately; if the queue is full - i.e. `gdrived` has stopped answering -
  the request is rejected straight away and reported in `statusMessage`
  rather than piling up.
- Requests are handled one at a time, so a "mutate, then refresh" pair stays
  in order, and each call is bounded by a 30s timeout (10s for connecting).
  A transport failure or timeout drops the connection so the next request
  reconnects, which is how the UI recovers from `gdrived` restarting.
- Replies are delivered back to the Qt thread with `CxxQtThread::queue`,
  which is where properties are updated and signals emitted. Results carry a
  generation number so a superseded reply can never overwrite newer state.
- `listDriveFolders()` returns nothing; its result arrives via the
  `driveFoldersReady(parentId, foldersJson)` signal.
- Sign-in completion is driven by `gdrived`'s `AuthenticationChanged` signal,
  not by polling. The QML refresh `Timer` remains only for sync status
  (`running` flags), which has no signal of its own.

## Dark mode

The UI follows the desktop's dark/light preference. At startup, `main.rs`
sets the Qt Quick Controls fallback style to "Fusion" (which draws every
control from the QML `palette` property, unlike the default "Basic" style),
and `dbus_client::query_prefers_dark()` queries the freedesktop desktop
portal's `org.freedesktop.appearance` `color-scheme` setting over D-Bus
(supported by `xdg-desktop-portal-kde`/`-gnome`/`-gtk` on effectively every
modern desktop, so this doesn't depend on a Qt6-specific platform theme
plugin being installed). `qml/main.qml` applies an approximate Breeze Dark
`Palette` to the window when `SyncManager.darkMode` is true. The preference
is only read once at startup (asynchronously, not live-updated if changed
while running).
