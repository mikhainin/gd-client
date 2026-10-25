//! The `SyncManager` `QObject` exposed to QML: a thin bridge between the
//! Qt/QML UI and `gdrived`'s D-Bus interface (see
//! `gdrive_common::dbus_api::GDrive1`). All the actual synchronisation logic
//! lives in `gdrived`/`gdrive-sync` - this object only lists/adds/removes/
//! toggles sync folders and reports the last error, if any, back to QML.

use cxx_qt::{CxxQtType, Threading};
use serde::Serialize;

use crate::dbus_client::{self, AuthenticationEvent, PendingRequest, Request, Response};

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

/// One row of `driveFoldersJson`: a Google Drive folder id and
/// display name, for the remote folder-browse dialog.
#[derive(Serialize)]
struct DriveFolderView {
    id: String,
    name: String,
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
        /// operation (including D-Bus/connection errors). `darkMode` is
        /// whether the desktop's dark color scheme preference was detected
        /// by the bounded startup request in `dbus_client`; the QML UI
        /// uses it to apply a matching palette, since Qt Quick Controls'
        /// styles don't reliably auto-detect this on Linux.
        #[qobject]
        #[qml_element]
        #[qproperty(QString, folders_json, cxx_name = "foldersJson")]
        #[qproperty(QString, status_message, cxx_name = "statusMessage")]
        #[qproperty(bool, connected)]
        #[qproperty(bool, authenticated)]
        #[qproperty(bool, dark_mode, cxx_name = "darkMode")]
        #[qproperty(bool, busy)]
        #[qproperty(bool, refreshing)]
        #[qproperty(QString, drive_folders_json, cxx_name = "driveFoldersJson")]
        #[qproperty(QString, drive_folders_error, cxx_name = "driveFoldersError")]
        #[qproperty(bool, drive_folders_loading, cxx_name = "driveFoldersLoading")]
        type SyncManager = super::SyncManagerRust;

        #[qinvokable]
        fn initialize(self: Pin<&mut Self>);

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

        /// Starts the interactive Google sign-in flow (opens the browser)
        /// and returns immediately; authentication signals update `authenticated`,
        /// with periodic snapshots retained as a fallback.
        #[qinvokable]
        #[cxx_name = "signIn"]
        fn sign_in(self: Pin<&mut Self>);

        /// Signs out, forgetting the cached token and stopping all sync
        /// folders, then refreshes.
        #[qinvokable]
        #[cxx_name = "signOut"]
        fn sign_out(self: Pin<&mut Self>);

        /// Starts a bounded background listing; results update driveFoldersJson.
        #[qinvokable]
        #[cxx_name = "listDriveFolders"]
        fn list_drive_folders(self: Pin<&mut Self>, parent_id: &QString);

        #[qinvokable]
        #[cxx_name = "cancelDriveFolders"]
        fn cancel_drive_folders(self: Pin<&mut Self>);

        /// Creates a new local sub-folder named `name` directly inside
        /// `parent_path`, for the "New folder" button in the local folder
        /// browser. Returns whether it succeeded (sets `statusMessage` with
        /// the reason on failure).
        #[qinvokable]
        #[cxx_name = "createLocalFolder"]
        fn create_local_folder(self: Pin<&mut Self>, parent_path: &QString, name: &QString)
            -> bool;

        /// Whether a native folder-picker command (`kdialog`) is available,
        /// so QML can prefer it over the built-in browser dialog.
        #[qinvokable]
        #[cxx_name = "nativeFolderPickerAvailable"]
        fn native_folder_picker_available(self: Pin<&mut Self>) -> bool;

        /// Runs `kdialog --getexistingdirectory` starting at `start_path`
        /// and returns the chosen path, or an empty string if the user
        /// cancelled. Only call this after checking
        /// [`Self::native_folder_picker_available`].
        #[qinvokable]
        #[cxx_name = "pickLocalFolderNative"]
        fn pick_local_folder_native(self: Pin<&mut Self>, start_path: &QString) -> QString;
    }

    impl cxx_qt::Threading for SyncManager {}
}

use core::pin::Pin;
use cxx_qt_lib::QString;

/// Rust-side state for the [`qobject::SyncManager`] `QObject`.
pub struct SyncManagerRust {
    folders_json: QString,
    status_message: QString,
    connected: bool,
    authenticated: bool,
    dark_mode: bool,
    busy: bool,
    refreshing: bool,
    drive_folders_json: QString,
    drive_folders_error: QString,
    drive_folders_loading: bool,
    initialized: bool,
    operation_message: Option<String>,
    waiting_for_sign_in: bool,
    snapshot: PendingRequest,
    mutation: PendingRequest,
    browse: PendingRequest,
    portal: PendingRequest,
    authentication_watch: PendingRequest,
    authentication_revision: u64,
}

impl Default for SyncManagerRust {
    fn default() -> Self {
        Self {
            folders_json: QString::default(),
            status_message: QString::default(),
            connected: false,
            authenticated: false,
            dark_mode: false,
            busy: false,
            refreshing: false,
            drive_folders_json: QString::from("[]"),
            drive_folders_error: QString::default(),
            drive_folders_loading: false,
            initialized: false,
            operation_message: None,
            waiting_for_sign_in: false,
            snapshot: PendingRequest::default(),
            mutation: PendingRequest::default(),
            browse: PendingRequest::default(),
            portal: PendingRequest::default(),
            authentication_watch: PendingRequest::default(),
            authentication_revision: 0,
        }
    }
}

impl qobject::SyncManager {
    fn dispatch(
        &self,
        request: Request,
        complete: impl FnOnce(Pin<&mut Self>, Result<Response, String>) + Send + 'static,
    ) -> Result<tokio::task::AbortHandle, String> {
        let qt_thread = self.qt_thread();
        dbus_client::spawn(async move {
            let result = dbus_client::execute(request).await;
            if let Err(error) = qt_thread.queue(move |object| complete(object, result)) {
                tracing::debug!("could not deliver D-Bus result to Qt: {error:?}");
            }
        })
    }

    pub fn initialize(mut self: Pin<&mut Self>) {
        if self.initialized {
            return;
        }
        self.as_mut().rust_mut().initialized = true;
        let qt_thread = self.qt_thread();
        match dbus_client::spawn(dbus_client::watch_authentication(move |event| {
            if let Err(error) = qt_thread.queue(move |mut object| {
                if let AuthenticationEvent::Changed(authenticated) = event {
                    let revision = object.authentication_revision.wrapping_add(1);
                    object.as_mut().rust_mut().authentication_revision = revision;
                    object.as_mut().set_authenticated(authenticated);
                    if object.waiting_for_sign_in {
                        object.as_mut().rust_mut().operation_message = None;
                        object.as_mut().rust_mut().waiting_for_sign_in = false;
                    }
                }
                object.as_mut().rust_mut().snapshot.cancel();
                object.as_mut().set_refreshing(false);
                object.refresh();
            }) {
                tracing::debug!("could not deliver authentication event to Qt: {error:?}");
            }
        })) {
            Ok(handle) => self.as_mut().rust_mut().authentication_watch.attach(handle),
            Err(error) => tracing::warn!("could not watch authentication: {error}"),
        }
        match self.dispatch(Request::DarkMode, |mut object, result| match result {
            Ok(Response::DarkMode(dark)) => object.as_mut().set_dark_mode(dark),
            Err(error) => tracing::warn!("could not read desktop color scheme: {error}"),
            _ => tracing::error!("unexpected response to desktop color scheme request"),
        }) {
            Ok(handle) => self.as_mut().rust_mut().portal.attach(handle),
            Err(error) => tracing::warn!("{error}"),
        }
        self.refresh();
    }

    pub fn refresh(mut self: Pin<&mut Self>) {
        if self.busy || self.refreshing {
            return;
        }
        let generation = self.as_mut().rust_mut().snapshot.begin();
        let authentication_revision = self.authentication_revision;
        self.as_mut().set_refreshing(true);
        match self.dispatch(Request::Refresh, move |mut object, result| {
            if !object.as_mut().rust_mut().snapshot.finish(generation) {
                return;
            }
            object.as_mut().set_refreshing(false);
            let result = match result {
                Ok(Response::Snapshot(rows, authenticated)) => {
                    let folders: Vec<SyncFolderView> = rows
                        .into_iter()
                        .map(
                            |(
                                id,
                                display_name,
                                drive_folder_id,
                                local_path,
                                owner_user,
                                owner_group,
                                enabled,
                                running,
                            )| {
                                SyncFolderView {
                                    id,
                                    display_name,
                                    drive_folder_id,
                                    local_path,
                                    owner_user,
                                    owner_group,
                                    enabled,
                                    running,
                                }
                            },
                        )
                        .collect();
                    serde_json::to_string(&folders)
                        .map(|json| (json, folders.len(), authenticated))
                        .map_err(|e| format!("could not encode sync folders: {e}"))
                }
                Err(error) => Err(error),
                _ => Err("unexpected response to sync-folder request".to_string()),
            };
            match result {
                Ok((json, count, authenticated)) => {
                    object.as_mut().set_folders_json(QString::from(&json));
                    let mut authentication_error = None;
                    if object.authentication_revision == authentication_revision {
                        match authenticated {
                            Ok(authenticated) => {
                                object.as_mut().set_authenticated(authenticated);
                                if authenticated && object.waiting_for_sign_in {
                                    object.as_mut().rust_mut().operation_message = None;
                                    object.as_mut().rust_mut().waiting_for_sign_in = false;
                                }
                            }
                            Err(error) => {
                                tracing::warn!("authentication status unavailable: {error}");
                                authentication_error = Some(error);
                            }
                        }
                    }
                    object.as_mut().set_connected(true);
                    let message = object
                        .operation_message
                        .clone()
                        .unwrap_or_else(|| format!("{count} sync folder(s)"));
                    let message = match authentication_error {
                        Some(error) => {
                            format!("{message}; authentication status unavailable: {error}")
                        }
                        None => message,
                    };
                    object.as_mut().set_status_message(QString::from(&message));
                }
                Err(error) => {
                    object.as_mut().set_connected(false);
                    object.as_mut().set_status_message(QString::from(&error));
                }
            }
        }) {
            Ok(handle) => self.as_mut().rust_mut().snapshot.attach(handle),
            Err(error) => {
                self.as_mut().set_refreshing(false);
                self.as_mut().set_status_message(QString::from(&error));
            }
        }
    }

    fn mutate(mut self: Pin<&mut Self>, request: Request, success_message: Option<String>) {
        if self.busy {
            self.as_mut().set_status_message(QString::from(
                "another daemon operation is still in progress",
            ));
            return;
        }
        self.as_mut().rust_mut().snapshot.cancel();
        self.as_mut().set_refreshing(false);
        self.as_mut().rust_mut().operation_message = None;
        self.as_mut().rust_mut().waiting_for_sign_in = false;
        self.as_mut().set_busy(true);
        self.as_mut()
            .set_status_message(QString::from("Working..."));
        let generation = self.as_mut().rust_mut().mutation.begin();
        let signing_in = matches!(request, Request::SignIn);
        let authentication_revision = self.authentication_revision;
        match self.dispatch(request, move |mut object, result| {
            if !object.as_mut().rust_mut().mutation.finish(generation) {
                return;
            }
            object.as_mut().set_busy(false);
            let sign_in_started = signing_in && matches!(&result, Ok(Response::Done));
            let message = match result {
                Ok(Response::Done)
                    if signing_in && object.authentication_revision != authentication_revision =>
                {
                    None
                }
                Ok(Response::Done) => success_message,
                Err(error) => Some(error),
                _ => Some("unexpected response to daemon operation".to_string()),
            };
            if let Some(message) = &message {
                object.as_mut().set_status_message(QString::from(message));
            }
            object.as_mut().rust_mut().waiting_for_sign_in = sign_in_started && message.is_some();
            object.as_mut().rust_mut().operation_message = message;
            object.refresh();
        }) {
            Ok(handle) => self.as_mut().rust_mut().mutation.attach(handle),
            Err(error) => {
                self.as_mut().set_busy(false);
                self.as_mut().set_status_message(QString::from(&error));
            }
        }
    }

    pub fn add_folder(
        self: Pin<&mut Self>,
        display_name: &QString,
        drive_folder_id: &QString,
        local_path: &QString,
        owner_user: &QString,
        owner_group: &QString,
        enabled: bool,
    ) {
        self.mutate(
            Request::AddFolder {
                display_name: display_name.to_string(),
                drive_folder_id: drive_folder_id.to_string(),
                local_path: local_path.to_string(),
                owner_user: owner_user.to_string(),
                owner_group: owner_group.to_string(),
                enabled,
            },
            None,
        );
    }

    pub fn remove_folder(self: Pin<&mut Self>, id: &QString) {
        self.mutate(Request::RemoveFolder(id.to_string()), None);
    }

    pub fn set_folder_enabled(self: Pin<&mut Self>, id: &QString, enabled: bool) {
        self.mutate(Request::SetFolderEnabled(id.to_string(), enabled), None);
    }

    pub fn sign_in(self: Pin<&mut Self>) {
        self.mutate(
            Request::SignIn,
            Some("opened browser for Google sign-in; waiting for you to finish...".to_string()),
        );
    }

    pub fn sign_out(self: Pin<&mut Self>) {
        self.mutate(Request::SignOut, None);
    }

    pub fn list_drive_folders(mut self: Pin<&mut Self>, parent_id: &QString) {
        let generation = self.as_mut().rust_mut().browse.begin();
        self.as_mut().set_drive_folders_json(QString::from("[]"));
        self.as_mut().set_drive_folders_error(QString::default());
        self.as_mut().set_drive_folders_loading(true);
        match self.dispatch(
            Request::ListDriveFolders(parent_id.to_string()),
            move |mut object, result| {
                if !object.as_mut().rust_mut().browse.finish(generation) {
                    return;
                }
                let json = match result {
                    Ok(Response::DriveFolders(rows)) => {
                        let folders: Vec<DriveFolderView> = rows
                            .into_iter()
                            .map(|(id, name)| DriveFolderView { id, name })
                            .collect();
                        serde_json::to_string(&folders)
                            .map_err(|e| format!("could not encode Drive folders: {e}"))
                    }
                    Err(error) => Err(error),
                    _ => Err("unexpected response to Drive-folder request".to_string()),
                };
                match json {
                    Ok(json) => object.as_mut().set_drive_folders_json(QString::from(&json)),
                    Err(error) => object
                        .as_mut()
                        .set_drive_folders_error(QString::from(&error)),
                }
                object.as_mut().set_drive_folders_loading(false);
            },
        ) {
            Ok(handle) => self.as_mut().rust_mut().browse.attach(handle),
            Err(error) => {
                self.as_mut().set_drive_folders_error(QString::from(&error));
                self.as_mut().set_drive_folders_loading(false);
            }
        }
    }

    pub fn cancel_drive_folders(mut self: Pin<&mut Self>) {
        self.as_mut().rust_mut().browse.cancel();
        self.as_mut().set_drive_folders_loading(false);
    }

    pub fn create_local_folder(
        mut self: Pin<&mut Self>,
        parent_path: &QString,
        name: &QString,
    ) -> bool {
        let parent_path = parent_path.to_string();
        let name = name.to_string();

        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('/')
            || name.contains('\\')
        {
            self.as_mut().set_status_message(QString::from(
                "folder name must be non-empty and cannot be '.'/'..' or contain path separators",
            ));
            return false;
        }

        let full_path = std::path::Path::new(&parent_path).join(&name);
        match std::fs::create_dir(&full_path) {
            Ok(()) => true,
            Err(err) => {
                self.as_mut().set_status_message(QString::from(&format!(
                    "could not create {}: {err}",
                    full_path.display()
                )));
                false
            }
        }
    }

    pub fn native_folder_picker_available(self: Pin<&mut Self>) -> bool {
        std::process::Command::new("kdialog")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    pub fn pick_local_folder_native(mut self: Pin<&mut Self>, start_path: &QString) -> QString {
        use std::os::unix::process::CommandExt;

        let start_path = start_path.to_string();
        let mut command = std::process::Command::new("kdialog");
        command.arg("--getexistingdirectory");
        if !start_path.is_empty() {
            command.arg(&start_path);
        }

        // Ask the kernel to send kdialog a SIGTERM if this process dies
        // before it exits (e.g. gdrive-ui is killed while the picker is
        // still open), so it doesn't linger as an orphan.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        match command.output() {
            Ok(output) if output.status.success() => {
                QString::from(String::from_utf8_lossy(&output.stdout).trim())
            }
            // Non-zero exit means the user cancelled the dialog - not an
            // error, just nothing selected.
            Ok(_) => QString::from(""),
            Err(err) => {
                self.as_mut()
                    .set_status_message(QString::from(&format!("failed to run kdialog: {err}")));
                QString::from("")
            }
        }
    }
}
