//! The `SyncManager` `QObject` exposed to QML: a thin bridge between the
//! Qt/QML UI and `gdrived`'s D-Bus interface (see
//! `gdrive_common::dbus_api::GDrive1`). All the actual synchronisation logic
//! lives in `gdrived`/`gdrive-sync` - this object only lists/adds/removes/
//! toggles sync folders and reports the last error, if any, back to QML.
//!
//! Every D-Bus operation is asynchronous: the `#[qinvokable]` methods only
//! hand a request to the shared worker (see [`crate::dbus_client`]) and
//! return immediately, so the Qt event loop is never blocked. When the reply
//! arrives, the worker calls back on its own thread and the result is queued
//! onto the Qt thread with `CxxQtThread::queue`, which is where the
//! `Q_PROPERTY`s are updated and signals emitted - the only thread-safe way
//! to touch a `QObject` from outside its own thread.

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

/// One row of the `driveFoldersReady` signal's JSON: a Google Drive folder id
/// and display name, for the remote folder-browse dialog.
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
        /// (see `dbus_client::query_prefers_dark`); the QML UI uses it to
        /// apply a matching palette, since Qt Quick Controls' styles don't
        /// reliably auto-detect this on Linux.
        ///
        /// None of the invokables below wait for D-Bus: they return at once
        /// and the properties/signals update when the reply arrives.
        #[qobject]
        #[qml_element]
        #[qproperty(QString, folders_json, cxx_name = "foldersJson")]
        #[qproperty(QString, status_message, cxx_name = "statusMessage")]
        #[qproperty(bool, connected)]
        #[qproperty(bool, authenticated)]
        #[qproperty(bool, dark_mode, cxx_name = "darkMode")]
        type SyncManager = super::SyncManagerRust;

        /// Requests a fresh list of sync folders (and the authentication
        /// state) from `gdrived`; `foldersJson`, `connected`, `authenticated`
        /// and `statusMessage` update once the replies arrive.
        #[qinvokable]
        fn refresh(self: Pin<&mut Self>);

        /// Adds a new sync folder, refreshing the list once the daemon has
        /// acknowledged it.
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

        /// Removes a sync folder by id, refreshing the list afterwards.
        #[qinvokable]
        #[cxx_name = "removeFolder"]
        fn remove_folder(self: Pin<&mut Self>, id: &QString);

        /// Enables/disables a sync folder by id, refreshing afterwards.
        #[qinvokable]
        #[cxx_name = "setFolderEnabled"]
        fn set_folder_enabled(self: Pin<&mut Self>, id: &QString, enabled: bool);

        /// Starts the interactive Google sign-in flow (opens the browser).
        /// `authenticated` updates on its own once the flow finishes, driven
        /// by `gdrived`'s `AuthenticationChanged` signal.
        #[qinvokable]
        #[cxx_name = "signIn"]
        fn sign_in(self: Pin<&mut Self>);

        /// Signs out, forgetting the cached token and stopping all sync
        /// folders, then refreshes.
        #[qinvokable]
        #[cxx_name = "signOut"]
        fn sign_out(self: Pin<&mut Self>);

        /// Requests the direct sub-folders of a Google Drive folder (empty
        /// string for "My Drive"'s top level) for the remote folder-browse
        /// dialog. The result is delivered by the `driveFoldersReady`
        /// signal; only the most recently requested parent is reported, so
        /// quickly clicking through folders cannot show stale contents.
        #[qinvokable]
        #[cxx_name = "listDriveFolders"]
        fn list_drive_folders(self: Pin<&mut Self>, parent_id: &QString);

        /// Creates a new local sub-folder named `name` directly inside
        /// `parent_path`, for the "New folder" button in the local folder
        /// browser. Returns whether it succeeded (sets `statusMessage` with
        /// the reason on failure). Purely local, so it stays synchronous.
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

    extern "RustQt" {
        /// Emitted when a `listDriveFolders()` request completes, with the
        /// parent folder id it was made for and a JSON array of `{id, name}`
        /// objects (empty on failure, in which case `statusMessage` explains
        /// why).
        #[qsignal]
        #[cxx_name = "driveFoldersReady"]
        fn drive_folders_ready(
            self: Pin<&mut SyncManager>,
            parent_id: &QString,
            folders_json: &QString,
        );
    }

    // Lets background D-Bus replies queue closures onto the Qt event loop
    // (`CxxQtThread::queue`) instead of touching the QObject directly.
    impl cxx_qt::Threading for SyncManager {}
}

use core::pin::Pin;
use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::QString;
use gdrive_common::dbus_api::GDrive1Proxy;

/// Rust-side state for the [`qobject::SyncManager`] `QObject`.
#[derive(Default)]
pub struct SyncManagerRust {
    folders_json: QString,
    status_message: QString,
    connected: bool,
    authenticated: bool,
    dark_mode: bool,
    /// Bumped by every `refresh()`; a reply whose generation no longer
    /// matches belongs to a superseded refresh and is discarded rather than
    /// overwriting newer state.
    refresh_generation: u64,
    /// Same idea for `listDriveFolders()`, so navigating into a sub-folder
    /// before the previous listing arrives can't repopulate the dialog with
    /// the folder the user just left.
    drive_folders_generation: u64,
    /// Bumped whenever `gdrived`'s `AuthenticationChanged` signal updates
    /// `authenticated`, so an older polled `IsAuthenticated` reply can't
    /// revert it.
    authentication_generation: u64,
    /// Whether the one-off startup work (dark-mode query, signal
    /// subscription) has been done.
    started: bool,
}

impl qobject::SyncManager {
    pub fn refresh(mut self: Pin<&mut Self>) {
        self.as_mut().start();

        let generation = self.as_ref().rust().refresh_generation.wrapping_add(1);
        self.as_mut().rust_mut().refresh_generation = generation;

        let qt_thread = self.as_ref().qt_thread();
        let submitted = dbus_client::call(
            |proxy| async move { proxy.list_sync_folders().await },
            move |result| {
                let _ = qt_thread.queue(move |mut qobject| {
                    if qobject.as_ref().rust().refresh_generation != generation {
                        return;
                    }

                    match result {
                        Ok(rows) => {
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
                            let json = serde_json::to_string(&folders)
                                .unwrap_or_else(|_| "[]".to_string());
                            qobject.as_mut().set_folders_json(QString::from(&json));
                            qobject.as_mut().set_status_message(QString::from(&format!(
                                "{} sync folder(s)",
                                folders.len()
                            )));
                            qobject.as_mut().set_connected(true);
                        }
                        Err(message) => {
                            qobject.as_mut().set_folders_json(QString::from("[]"));
                            qobject.as_mut().set_status_message(QString::from(&message));
                            qobject.as_mut().set_connected(false);
                        }
                    }
                });
            },
        );
        if let Err(error) = submitted {
            self.as_mut().report(&error.to_string());
        }

        let authentication_generation = self.as_ref().rust().authentication_generation;
        let qt_thread = self.as_ref().qt_thread();
        let submitted = dbus_client::call(
            |proxy| async move { proxy.is_authenticated().await },
            move |result| {
                let _ = qt_thread.queue(move |mut qobject| {
                    // A newer `AuthenticationChanged` signal has already told
                    // us the answer; don't undo it with a polled result that
                    // may have been computed before it.
                    if qobject.as_ref().rust().authentication_generation
                        != authentication_generation
                    {
                        return;
                    }
                    qobject.as_mut().set_authenticated(result.unwrap_or(false));
                });
            },
        );
        if let Err(error) = submitted {
            self.as_mut().report(&error.to_string());
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

        self.as_mut().submit(
            move |proxy| async move {
                proxy
                    .add_sync_folder(
                        &display_name,
                        &drive_folder_id,
                        &local_path,
                        &owner_user,
                        &owner_group,
                        enabled,
                    )
                    .await
                    .map(|_id| ())
            },
            "adding the sync folder",
        );
    }

    pub fn remove_folder(mut self: Pin<&mut Self>, id: &QString) {
        let id = id.to_string();
        self.as_mut().submit(
            move |proxy| async move { proxy.remove_sync_folder(&id).await },
            "removing the sync folder",
        );
    }

    pub fn set_folder_enabled(mut self: Pin<&mut Self>, id: &QString, enabled: bool) {
        let id = id.to_string();
        self.as_mut().submit(
            move |proxy| async move { proxy.set_folder_enabled(&id, enabled).await },
            "updating the sync folder",
        );
    }

    pub fn sign_in(mut self: Pin<&mut Self>) {
        self.as_mut()
            .report("opening the browser for Google sign-in...");
        self.as_mut().submit(
            |proxy| async move { proxy.sign_in().await },
            "starting the sign-in flow",
        );
    }

    pub fn sign_out(mut self: Pin<&mut Self>) {
        self.as_mut()
            .submit(|proxy| async move { proxy.sign_out().await }, "signing out");
    }

    pub fn list_drive_folders(mut self: Pin<&mut Self>, parent_id: &QString) {
        self.as_mut().start();

        let generation = self
            .as_ref()
            .rust()
            .drive_folders_generation
            .wrapping_add(1);
        self.as_mut().rust_mut().drive_folders_generation = generation;

        let parent_id = parent_id.to_string();
        let requested_parent = parent_id.clone();
        let qt_thread = self.as_ref().qt_thread();
        let submitted = dbus_client::call(
            move |proxy| async move { proxy.list_drive_folders(&parent_id).await },
            move |result| {
                let _ = qt_thread.queue(move |mut qobject| {
                    if qobject.as_ref().rust().drive_folders_generation != generation {
                        return;
                    }

                    let json = match result {
                        Ok(rows) => {
                            let folders: Vec<DriveFolderView> = rows
                                .into_iter()
                                .map(|(id, name)| DriveFolderView { id, name })
                                .collect();
                            serde_json::to_string(&folders).unwrap_or_else(|_| "[]".to_string())
                        }
                        Err(message) => {
                            qobject.as_mut().set_status_message(QString::from(&message));
                            "[]".to_string()
                        }
                    };

                    qobject.as_mut().drive_folders_ready(
                        &QString::from(&requested_parent),
                        &QString::from(&json),
                    );
                });
            },
        );
        if let Err(error) = submitted {
            self.as_mut().report(&error.to_string());
        }
    }

    pub fn create_local_folder(
        mut self: Pin<&mut Self>,
        parent_path: &QString,
        name: &QString,
    ) -> bool {
        let parent_path = parent_path.to_string();
        let name = name.to_string();

        if name.is_empty() || name.contains('/') {
            self.as_mut()
                .report("folder name must be non-empty and cannot contain '/'");
            return false;
        }

        let full_path = std::path::Path::new(&parent_path).join(&name);
        match std::fs::create_dir(&full_path) {
            Ok(()) => true,
            Err(err) => {
                self.as_mut()
                    .report(&format!("could not create {}: {err}", full_path.display()));
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
                    .report(&format!("failed to run kdialog: {err}"));
                QString::from("")
            }
        }
    }

    /// One-off startup work, done on the first D-Bus-backed invokable rather
    /// than in `Default` so the `QObject` (and therefore its `qt_thread()`)
    /// already exists: asks the desktop portal for the dark-mode preference
    /// and subscribes to `gdrived`'s `AuthenticationChanged` signal.
    fn start(mut self: Pin<&mut Self>) {
        if self.as_ref().rust().started {
            return;
        }
        self.as_mut().rust_mut().started = true;

        let qt_thread = self.as_ref().qt_thread();
        let _ = dbus_client::query_prefers_dark(move |dark| {
            let _ = qt_thread.queue(move |mut qobject| qobject.as_mut().set_dark_mode(dark));
        });

        let qt_thread = self.as_ref().qt_thread();
        dbus_client::set_authentication_listener(move |authenticated| {
            let _ = qt_thread.queue(move |mut qobject| {
                let generation = qobject
                    .as_ref()
                    .rust()
                    .authentication_generation
                    .wrapping_add(1);
                qobject.as_mut().rust_mut().authentication_generation = generation;
                qobject.as_mut().set_authenticated(authenticated);
                // Sign-in/out starts or stops folders, so the list's
                // `running` flags are stale now.
                qobject.as_mut().refresh();
            });
        });
    }

    /// Submits a "mutate, then refresh" D-Bus request: `what` names the
    /// operation for the error message shown if it fails.
    fn submit<Fut>(
        mut self: Pin<&mut Self>,
        op: impl FnOnce(GDrive1Proxy<'static>) -> Fut + Send + 'static,
        what: &str,
    ) where
        Fut: std::future::Future<Output = zbus::Result<()>> + Send + 'static,
    {
        self.as_mut().start();

        let what = what.to_string();
        let qt_thread = self.as_ref().qt_thread();
        let submitted = dbus_client::call(op, move |result| {
            let _ = qt_thread.queue(move |mut qobject| {
                if let Err(message) = result {
                    qobject
                        .as_mut()
                        .set_status_message(QString::from(&format!("{what}: {message}")));
                }
                // Pick up whatever the daemon actually ended up with, both
                // on success and on failure.
                qobject.as_mut().refresh();
            });
        });
        if let Err(error) = submitted {
            self.as_mut().report(&format!("{what}: {error}"));
        }
    }

    fn report(mut self: Pin<&mut Self>, message: &str) {
        self.as_mut().set_status_message(QString::from(message));
    }
}
