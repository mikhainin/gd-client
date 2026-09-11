//! The `SyncManager` `QObject` exposed to QML: a thin bridge between the
//! Qt/QML UI and `gdrived`'s D-Bus interface (see
//! `gdrive_common::dbus_api::GDrive1`). All the actual synchronisation logic
//! lives in `gdrived`/`gdrive-sync` - this object only lists/adds/removes/
//! toggles sync folders and reports the last error, if any, back to QML.

use serde::Serialize;

use crate::dbus_client;

/// One row of `foldersJson`, mirroring [`gdrive_common::dbus_api::SyncFolderRow`]
/// but as a named struct so it serialises to a JSON object per folder (easier
/// to consume from QML/JS than a tuple-shaped array).
#[derive(Serialize)]
struct SyncFolderView {
    id: String,
    #[serde(rename = "displayName")]
    display_name: String,
    #[serde(rename = "driveFolderId")]
    drive_folder_id: String,
    #[serde(rename = "localPath")]
    local_path: String,
    #[serde(rename = "ownerUser")]
    owner_user: String,
    #[serde(rename = "ownerGroup")]
    owner_group: String,
    enabled: bool,
    running: bool,
}

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    extern "RustQt" {
        /// Bridges `gdrived`'s sync folders to QML: `foldersJson` is a JSON
        /// array of objects (id, displayName, driveFolderId, localPath,
        /// ownerUser, ownerGroup, enabled, running), refreshed by calling
        /// `refresh()`; `statusMessage` reports the outcome of the last
        /// operation (including D-Bus/connection errors).
        #[qobject]
        #[qml_element]
        #[qproperty(QString, folders_json)]
        #[qproperty(QString, status_message)]
        #[qproperty(bool, connected)]
        type SyncManager = super::SyncManagerRust;

        /// Re-fetches the list of sync folders from `gdrived` over D-Bus.
        #[qinvokable]
        fn refresh(self: Pin<&mut Self>);

        /// Adds a new sync folder, then refreshes the list.
        #[qinvokable]
        #[cxx_name = "addFolder"]
        fn add_folder(
            self: Pin<&mut Self>,
            display_name: &QString,
            drive_folder_id: &QString,
            local_path: &QString,
            owner_user: &QString,
            owner_group: &QString,
            enabled: bool,
        );

        /// Removes a sync folder by id, then refreshes the list.
        #[qinvokable]
        #[cxx_name = "removeFolder"]
        fn remove_folder(self: Pin<&mut Self>, id: &QString);

        /// Enables/disables a sync folder by id, then refreshes the list.
        #[qinvokable]
        #[cxx_name = "setFolderEnabled"]
        fn set_folder_enabled(self: Pin<&mut Self>, id: &QString, enabled: bool);
    }
}

use core::pin::Pin;
use cxx_qt_lib::QString;

/// Rust-side state for the [`qobject::SyncManager`] `QObject`.
#[derive(Default)]
pub struct SyncManagerRust {
    folders_json: QString,
    status_message: QString,
    connected: bool,
}

impl qobject::SyncManager {
    pub fn refresh(mut self: Pin<&mut Self>) {
        match dbus_client::with_proxy(|proxy| proxy.list_sync_folders()) {
            Ok(rows) => {
                let folders: Vec<SyncFolderView> = rows
                    .into_iter()
                    .map(|(id, display_name, drive_folder_id, local_path, owner_user, owner_group, enabled, running)| SyncFolderView {
                        id,
                        display_name,
                        drive_folder_id,
                        local_path,
                        owner_user,
                        owner_group,
                        enabled,
                        running,
                    })
                    .collect();
                let json = serde_json::to_string(&folders).unwrap_or_else(|_| "[]".to_string());
                self.as_mut().set_folders_json(QString::from(&json));
                self.as_mut().set_status_message(QString::from(&format!("{} sync folder(s)", folders.len())));
                self.as_mut().set_connected(true);
            }
            Err(message) => {
                self.as_mut().set_folders_json(QString::from("[]"));
                self.as_mut().set_status_message(QString::from(&message));
                self.as_mut().set_connected(false);
            }
        }
    }

    pub fn add_folder(
        mut self: Pin<&mut Self>,
        display_name: &QString,
        drive_folder_id: &QString,
        local_path: &QString,
        owner_user: &QString,
        owner_group: &QString,
        enabled: bool,
    ) {
        let display_name = display_name.to_string();
        let drive_folder_id = drive_folder_id.to_string();
        let local_path = local_path.to_string();
        let owner_user = owner_user.to_string();
        let owner_group = owner_group.to_string();

        let result = dbus_client::with_proxy(|proxy| {
            proxy.add_sync_folder(&display_name, &drive_folder_id, &local_path, &owner_user, &owner_group, enabled)
        });
        if let Err(message) = result {
            self.as_mut().set_status_message(QString::from(&message));
        }
        self.refresh();
    }

    pub fn remove_folder(mut self: Pin<&mut Self>, id: &QString) {
        let id = id.to_string();
        let result = dbus_client::with_proxy(|proxy| proxy.remove_sync_folder(&id));
        if let Err(message) = result {
            self.as_mut().set_status_message(QString::from(&message));
        }
        self.refresh();
    }

    pub fn set_folder_enabled(mut self: Pin<&mut Self>, id: &QString, enabled: bool) {
        let id = id.to_string();
        let result = dbus_client::with_proxy(|proxy| proxy.set_folder_enabled(&id, enabled));
        if let Err(message) = result {
            self.as_mut().set_status_message(QString::from(&message));
        }
        self.refresh();
    }
}
