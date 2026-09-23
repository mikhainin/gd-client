# gdrive-ui

Qt6/QML desktop control panel for `gdrived`, built with
[cxx-qt](https://github.com/KDAB/cxx-qt) (no CMake required).

- `cxxqt_object.rs` — the `SyncManager` `QObject` exposed to QML: lists,
  adds, removes, and enables/disables sync folders by calling `gdrived`'s
  D-Bus interface (`gdrive_common::dbus_api::GDrive1`), exposing a JSON
  `foldersJson` property for the QML `ListView` model, plus a `darkMode`
  property (see `dbus_client::system_prefers_dark`).
- `dbus_client.rs` — thin helper for talking to `gdrived` over D-Bus
  (`GDrive1ProxyBlocking`, auto-generated from the shared `GDrive1` trait),
  plus `system_prefers_dark()`, which asks the freedesktop desktop portal
  (`org.freedesktop.portal.Desktop`) for the user's dark/light color-scheme
  preference.
- `qml/main.qml` — the UI itself: a folder list with add/remove/enable
  controls, deliberately avoiding `QtQuick.Layouts` (not always packaged
  alongside base QtQuick Controls) in favour of plain `Row`/`Column`. Also
  includes: a Sign in/out button with a 3s polling `Timer` to pick up
  async sign-in completion; a `Qt.labs.platform.SystemTrayIcon` (closing
  the window hides it to tray instead of quitting; Show/Hide + Quit menu);
  a custom local folder browser `Dialog` (backed by
  `Qt.labs.folderlistmodel`, with a "New folder…" button) for the local
  path field; and a custom breadcrumb-navigable Drive folder browser
  `Dialog` (backed by `listDriveFolders`) for the Drive folder id field.
  `Qt.labs.platform.FolderDialog` and `QtQuick.Dialogs`' `FolderDialog`
  were tried first for a native picker but rejected: the former needs
  `QApplication`/QtWidgets (this app is a pure-QML `QGuiApplication` and
  doesn't link against it, so it fails at runtime with "No native
  FileDialog implementation available"), and the latter works but has no
  "new folder" affordance.
- `build.rs` — builds the QML module (`org.gclient.gdrive_ui`) via
  `cxx-qt-build`.

## Local folder picker: native dialog first

The Local path "Browse…" button calls `SyncManager::nativeFolderPickerAvailable()`
and, if true, `pickLocalFolderNative(start_path)` (both in `cxxqt_object.rs`),
which shell out to `kdialog --getexistingdirectory` (a real KDE-native
`QFileDialog`, run as a separate process so this app doesn't need to link
`QApplication`/QtWidgets itself). `kdialog` is an optional/recommended
dependency, not a hard one: when it isn't installed, the button falls back
to the custom `FolderListModel`-based `localFolderDialog` described above.
The spawned `kdialog` process has `PR_SET_PDEATHSIG` (via the `libc` crate's
`pre_exec` hook) set to `SIGTERM` before `exec`, so it's killed automatically
if `gdrive-ui` itself dies while the picker is open.

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

## Dark mode

The UI follows the desktop's dark/light preference. At startup, `main.rs`
sets the Qt Quick Controls fallback style to "Fusion" (which draws every
control from the QML `palette` property, unlike the default "Basic" style),
and `dbus_client::system_prefers_dark()` queries the freedesktop desktop
portal's `org.freedesktop.appearance` `color-scheme` setting over D-Bus
(supported by `xdg-desktop-portal-kde`/`-gnome`/`-gtk` on effectively every
modern desktop, so this doesn't depend on a Qt6-specific platform theme
plugin being installed). `qml/main.qml` applies an approximate Breeze Dark
`Palette` to the window when `SyncManager.darkMode` is true. The preference
is only read once at startup (not live-updated if changed while running).
