# gdrive-ui

Qt6/QML desktop control panel for `gdrived`, built with
[cxx-qt](https://github.com/KDAB/cxx-qt) (no CMake required).

- `cxxqt_object.rs` — the `SyncManager` `QObject` exposed to QML: lists,
  adds, removes, and enables/disables sync folders by calling `gdrived`'s
  D-Bus interface (`gdrive_common::dbus_api::GDrive1`), exposing a JSON
  `foldersJson` property for the QML `ListView` model.
- `dbus_client.rs` — thin helper for talking to `gdrived` over D-Bus
  (`GDrive1ProxyBlocking`, auto-generated from the shared `GDrive1` trait).
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
