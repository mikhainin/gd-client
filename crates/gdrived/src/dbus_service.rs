//! D-Bus service exposed by the daemon so the `gdrive-ui` Qt application (or
//! any other client, e.g. `busctl`) can inspect and control synchronisation
//! without talking to the engine/config directly.
//!
//! Bus name: `org.gclient.GDrive1`, object path: `/org/gclient/GDrive1`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gdrive_common::dbus_api::{DriveFolderRow, SyncFolderRow, SyncStatus};
use gdrive_common::{local_roots, AppConfig, CommonError, SyncFolder};
use gdrive_sync::auth::{self, OAuthConfig};
use gdrive_sync::{SyncEngine, SyncError};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinHandle;
use uuid::Uuid;
use zbus::interface;
use zbus::object_server::SignalEmitter;

const SIGNAL_TIMEOUT: Duration = Duration::from_secs(2);
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const SIGN_IN_CANCEL_TIMEOUT: Duration = Duration::from_secs(2);
const ROOT_INSPECTION_TIMEOUT: Duration = Duration::from_secs(3);

struct ServiceState {
    config: AppConfig,
    sign_in_generation: u64,
    session_revision: u64,
    sign_in: Option<JoinHandle<()>>,
    cancelling_sign_in: bool,
}

impl ServiceState {
    fn complete_sign_in(
        &mut self,
        generation: u64,
        token: auth::StoredToken,
        token_path: &std::path::Path,
        engine: &SyncEngine,
    ) -> Result<bool, SyncError> {
        if self.sign_in_generation != generation {
            return Ok(false);
        }
        self.sign_in = None;
        engine.save_token(token, token_path)?;
        self.session_revision = self.session_revision.wrapping_add(1);
        Ok(true)
    }

    fn sign_in_busy(&self) -> bool {
        self.cancelling_sign_in
            || self
                .sign_in
                .as_ref()
                .is_some_and(|task| !task.is_finished())
    }

    fn cancel_sign_in(&mut self) -> Option<JoinHandle<()>> {
        if self.cancelling_sign_in {
            return None;
        }
        self.sign_in_generation = self.sign_in_generation.wrapping_add(1);
        self.sign_in.take().inspect(|task| {
            task.abort();
            self.cancelling_sign_in = true;
        })
    }
}

/// Shared daemon state backing the D-Bus interface: the persisted
/// configuration (guarded by a mutex since D-Bus calls are handled
/// concurrently) and the running sync engine.
#[derive(Clone)]
pub struct GDriveService {
    state: Arc<Mutex<ServiceState>>,
    config_path: PathBuf,
    token_path: PathBuf,
    engine: SyncEngine,
    root_inspection: Arc<Semaphore>,
}

impl GDriveService {
    pub async fn forward_status(
        connection: zbus::Connection,
        mut changes: tokio::sync::watch::Receiver<u64>,
    ) {
        while changes.changed().await.is_ok() {
            let revision = *changes.borrow_and_update();
            match tokio::time::timeout(
                SIGNAL_TIMEOUT,
                connection.emit_signal(
                    None::<&str>,
                    gdrive_common::dbus_api::OBJECT_PATH,
                    "org.gclient.GDrive1",
                    "SyncStatusChanged",
                    &(revision,),
                ),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!("failed to emit SyncStatusChanged: {error}"),
                Err(_) => tracing::warn!("SyncStatusChanged emission timed out"),
            }
        }
    }

    pub fn new(
        config: AppConfig,
        config_path: PathBuf,
        token_path: PathBuf,
        engine: SyncEngine,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(ServiceState {
                config,
                sign_in_generation: 0,
                session_revision: 0,
                sign_in: None,
                cancelling_sign_in: false,
            })),
            config_path,
            token_path,
            engine,
            root_inspection: Arc::new(Semaphore::new(1)),
        }
    }

    fn persist(&self, config: &AppConfig) -> zbus::fdo::Result<()> {
        config
            .save_to(&self.config_path)
            .map_err(|e| zbus::fdo::Error::Failed(format!("failed to save configuration: {e}")))
    }

    async fn inspect_roots(
        &self,
        config: &AppConfig,
        deadline: tokio::time::Instant,
    ) -> Result<Vec<(Uuid, CommonError)>, SyncError> {
        if !config.sync_folders.iter().any(|folder| folder.enabled) {
            return Ok(Vec::new());
        }
        let folders = config.sync_folders.clone();
        let gate = Arc::clone(&self.root_inspection);
        tokio::time::timeout_at(deadline, async move {
            let permit = gate.acquire_owned().await.map_err(std::io::Error::other)?;
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                local_roots::errors(&folders)
            })
            .await
            .map_err(|error| SyncError::Io(std::io::Error::other(error)))
        })
        .await
        .map_err(|source| SyncError::Timeout {
            operation: "inspecting local sync roots",
            source: Box::new(source),
        })?
    }

    async fn validate_candidate(&self, config: &AppConfig) -> zbus::fdo::Result<()> {
        let errors = self
            .inspect_roots(
                config,
                tokio::time::Instant::now() + ROOT_INSPECTION_TIMEOUT,
            )
            .await
            .map_err(|error| {
                tracing::warn!(%error, "local root inspection failed before configuration commit");
                if matches!(error, SyncError::Timeout { .. }) {
                    zbus::fdo::Error::Timeout(error.to_string())
                } else {
                    zbus::fdo::Error::Failed(error.to_string())
                }
            })?;
        if let Some((_, error)) = errors.first() {
            return Err(zbus::fdo::Error::InvalidArgs(error.to_string()));
        }
        Ok(())
    }

    fn unchanged(
        state: &ServiceState,
        original: &AppConfig,
        session_revision: u64,
    ) -> zbus::fdo::Result<()> {
        if state.config != *original || state.session_revision != session_revision {
            return Err(zbus::fdo::Error::Failed(
                "configuration or authentication session changed during local root inspection; retry the operation".into(),
            ));
        }
        Ok(())
    }

    pub async fn start_configured_folders(&self, session_revision: u64, only_blocked: bool) {
        let deadline = tokio::time::Instant::now() + ROOT_INSPECTION_TIMEOUT;
        for _ in 0..3 {
            let original = {
                let state = self.state.lock().await;
                if state.session_revision != session_revision {
                    return;
                }
                state.config.clone()
            };
            let result = self.inspect_roots(&original, deadline).await;
            let state = self.state.lock().await;
            if state.session_revision != session_revision {
                return;
            }
            if state.config != original {
                continue;
            }
            match result {
                Ok(errors) if only_blocked => {
                    self.engine.reconcile_roots_inspected(&original, errors)
                }
                Ok(errors) => self.engine.start_all_inspected(&original, errors),
                Err(error) => {
                    tracing::error!(%error, "local sync root inspection failed");
                    self.engine
                        .root_inspection_failed(&original, &error, only_blocked);
                }
            }
            return;
        }
        let state = self.state.lock().await;
        if state.session_revision == session_revision {
            let error = SyncError::Io(std::io::Error::new(std::io::ErrorKind::WouldBlock,
                "configuration kept changing during local root inspection; retry enabling the folder"));
            tracing::warn!(%error, "local sync root inspection interrupted");
            self.engine
                .root_inspection_failed(&state.config, &error, only_blocked);
        }
    }

    async fn finish_sign_in_cancellation(
        &self,
        task: Option<JoinHandle<()>>,
        deadline: Duration,
    ) -> zbus::fdo::Result<()> {
        let Some(mut task) = task else { return Ok(()) };
        let result = tokio::time::timeout(deadline, &mut task).await;
        let mut state = self.state.lock().await;
        state.cancelling_sign_in = false;
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) if error.is_cancelled() => Ok(()),
            Ok(Err(_)) => Err(zbus::fdo::Error::Failed(
                "sign-in task failed during cancellation".into(),
            )),
            Err(_) => {
                state.sign_in = Some(task);
                Err(zbus::fdo::Error::Timeout(
                    "sign-in cancellation is still in progress; retry shortly".into(),
                ))
            }
        }
    }

    async fn notify_authentication(emitter: &SignalEmitter<'_>, authenticated: bool) {
        Self::notify(
            "AuthenticationChanged",
            Self::authentication_changed(emitter, authenticated),
        )
        .await;
    }

    async fn notify(name: &str, signal: impl std::future::Future<Output = zbus::Result<()>>) {
        match tokio::time::timeout(SIGNAL_TIMEOUT, signal).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(signal = name, %error, "authentication signal emission failed")
            }
            Err(_) => tracing::warn!(signal = name, "authentication signal emission timed out"),
        }
    }

    async fn finish_sign_in(
        &self,
        generation: u64,
        result: Result<auth::StoredToken, SyncError>,
        emitter: &SignalEmitter<'_>,
    ) {
        let mut state = self.state.lock().await;
        if state.sign_in_generation != generation {
            return;
        }
        state.sign_in = None;
        match result.and_then(|token| {
            state.complete_sign_in(generation, token, &self.token_path, &self.engine)
        }) {
            Ok(true) => {
                tracing::info!("sign-in succeeded");
                Self::notify_authentication(emitter, true).await;
                let session_revision = state.session_revision;
                drop(state);
                self.start_configured_folders(session_revision, false).await;
            }
            Ok(false) => {}
            Err(error) => {
                tracing::error!(%error, "sign-in failed");
                let message = if matches!(&error, SyncError::Timeout { .. }) {
                    "Sign-in timed out. Please try again."
                } else if matches!(error, SyncError::OAuthDeclined) {
                    "Google authorization was declined. Please try again."
                } else {
                    "Sign-in failed. Check daemon logs and try again."
                };
                Self::notify("SignInFailed", Self::sign_in_failed(emitter, message)).await;
            }
        }
    }
}

/// Implements the `org.gclient.GDrive1` interface defined in
/// `gdrive_common::dbus_api` (see [`gdrive_common::dbus_api::GDrive1`] for
/// the client-side proxy consumed by `gdrive-ui`). The method
/// signatures here must stay in sync with that trait.
#[interface(name = "org.gclient.GDrive1")]
impl GDriveService {
    fn sync_status(&self) -> SyncStatus {
        self.engine.sync_status()
    }

    #[zbus(signal)]
    async fn sync_status_changed(emitter: &SignalEmitter<'_>, revision: u64) -> zbus::Result<()>;

    /// Lists all configured sync folders and whether each is currently active.
    async fn list_sync_folders(&self) -> Vec<SyncFolderRow> {
        let state = self.state.lock().await;
        let running = self.engine.running_folders();
        state
            .config
            .sync_folders
            .iter()
            .map(|f| {
                (
                    f.id.to_string(),
                    f.display_name.clone(),
                    f.drive_folder_id.clone().unwrap_or_default(),
                    f.drive_folder_path.clone().unwrap_or_default(),
                    f.local_path.to_string_lossy().to_string(),
                    f.owner_user.clone(),
                    f.owner_group.clone(),
                    f.enabled,
                    running.contains(&f.id),
                )
            })
            .collect()
    }

    /// Adds a new sync folder and, if `enabled` is true, starts synchronising
    /// it immediately. Returns the new folder's generated id.
    #[allow(clippy::too_many_arguments)]
    async fn add_sync_folder(
        &self,
        display_name: String,
        drive_folder_id: String,
        drive_folder_path: String,
        local_path: String,
        owner_user: String,
        owner_group: String,
        enabled: bool,
    ) -> zbus::fdo::Result<String> {
        let mut folder = SyncFolder::new(
            display_name,
            PathBuf::from(local_path),
            owner_user,
            owner_group,
        );
        folder.drive_folder_id = if drive_folder_id.is_empty() || drive_folder_id == "root" {
            None
        } else {
            Some(drive_folder_id)
        };
        folder.drive_folder_path = if folder.drive_folder_id.is_none() {
            Some("My Drive".into())
        } else if drive_folder_path.is_empty() {
            None
        } else {
            Some(drive_folder_path)
        };
        folder.enabled = enabled;
        let id = folder.id;

        let (original, session_revision) = {
            let state = self.state.lock().await;
            (state.config.clone(), state.session_revision)
        };
        let mut config = original.clone();
        config.sync_folders.push(folder.clone());
        if enabled {
            self.validate_candidate(&config).await?;
        }
        let mut state = self.state.lock().await;
        Self::unchanged(&state, &original, session_revision)?;
        self.persist(&config)?;
        state.config = config;

        if enabled {
            self.engine.start_folder(folder);
        }

        Ok(id.to_string())
    }

    /// Removes a sync folder and stops synchronisation under the state lock.
    async fn remove_sync_folder(&self, id: String) -> zbus::fdo::Result<()> {
        let folder_id = parse_uuid(&id)?;
        let mut state = self.state.lock().await;
        let mut config = state.config.clone();
        config.sync_folders.retain(|f| f.id != folder_id);
        self.persist(&config)?;
        state.config = config;
        self.engine.stop_folder_in_config(folder_id, &state.config);
        let session_revision = state.session_revision;
        drop(state);
        self.start_configured_folders(session_revision, true).await;
        Ok(())
    }

    /// Enables or disables a sync folder, starting/stopping its background
    /// synchronisation task accordingly.
    async fn set_folder_enabled(&self, id: String, enabled: bool) -> zbus::fdo::Result<()> {
        let folder_id = parse_uuid(&id)?;

        let (original, session_revision) = {
            let state = self.state.lock().await;
            (state.config.clone(), state.session_revision)
        };
        let mut config = original.clone();
        let folder = config
            .sync_folders
            .iter_mut()
            .find(|f| f.id == folder_id)
            .ok_or_else(|| zbus::fdo::Error::Failed(format!("no such sync folder: {id}")))?;
        folder.enabled = enabled;
        let folder = folder.clone();
        if enabled {
            self.validate_candidate(&config).await?;
        }
        let mut state = self.state.lock().await;
        Self::unchanged(&state, &original, session_revision)?;
        self.persist(&config)?;
        state.config = config;

        if enabled {
            self.engine.start_folder(folder);
        } else {
            self.engine.stop_folder_in_config(folder_id, &state.config);
            let session_revision = state.session_revision;
            drop(state);
            self.start_configured_folders(session_revision, true).await;
        }
        Ok(())
    }

    /// Returns whether a Google account is currently authenticated (i.e. a
    /// cached OAuth token exists).
    async fn is_authenticated(&self) -> zbus::fdo::Result<bool> {
        let _state = self.state.lock().await;
        let token = auth::load_token(&self.token_path)
            .map_err(|e| zbus::fdo::Error::Failed(format!("failed to load cached token: {e}")))?;
        Ok(token.is_some())
    }

    /// Starts the interactive Google OAuth sign-in flow in the background
    /// and returns immediately; the browser consent step can take an
    /// several minutes, so it must not block this D-Bus call (and
    /// therefore the caller's UI thread). Successful completion emits
    /// [`Self::authentication_changed`]; failures emit [`Self::sign_in_failed`].
    async fn sign_in(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let oauth_config = OAuthConfig::from_env().ok_or_else(|| {
            zbus::fdo::Error::Failed(
                "GDRIVE_CLIENT_ID/GDRIVE_CLIENT_SECRET are not set - cannot start the OAuth flow"
                    .to_string(),
            )
        })?;

        let mut state = self.state.lock().await;
        if state.sign_in_busy() {
            return Err(zbus::fdo::Error::Failed(
                "sign-in is already in progress".into(),
            ));
        }
        state.sign_in_generation = state.sign_in_generation.wrapping_add(1);
        let generation = state.sign_in_generation;
        let service = self.clone();
        let emitter = emitter.to_owned();
        let task = tokio::spawn(async move {
            let result = tokio::time::timeout(
                SIGN_IN_TIMEOUT,
                auth::authorize(&oauth_config, |url| {
                    if let Err(err) = std::process::Command::new("xdg-open").arg(url).spawn() {
                        tracing::warn!(
                        "failed to open browser for sign-in ({err}); open this URL manually: {url}"
                    );
                    } else {
                        tracing::info!("opened browser for Google sign-in: {url}");
                    }
                }),
            )
            .await
            .map_err(|source| SyncError::Timeout {
                operation: "sign-in exceeded 10 minutes",
                source: Box::new(source),
            })
            .and_then(|result| result);

            service.finish_sign_in(generation, result, &emitter).await;
        });
        state.sign_in = Some(task);

        Ok(())
    }

    /// Forgets the cached OAuth token and stops all running sync folders.
    async fn sign_out(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let (task, removal) = {
            let mut state = self.state.lock().await;
            state.session_revision = state.session_revision.wrapping_add(1);
            let task = state.cancel_sign_in();
            self.engine.set_access_token(String::new());
            let removal = if self.token_path.exists() {
                std::fs::remove_file(&self.token_path).map_err(|e| {
                    zbus::fdo::Error::Failed(format!("failed to remove cached token: {e}"))
                })
            } else {
                Ok(())
            };

            for folder in &state.config.sync_folders {
                self.engine.stop_folder(folder.id);
            }
            if removal.is_ok() {
                Self::notify_authentication(&emitter, false).await;
            }
            (task, removal)
        };
        let cancellation = self
            .finish_sign_in_cancellation(task, SIGN_IN_CANCEL_TIMEOUT)
            .await;
        removal?;
        cancellation?;
        Ok(())
    }

    /// Lists the direct sub-folders of a Google Drive folder, for the
    /// "browse Drive folder" picker in the UI.
    async fn list_drive_folders(
        &self,
        parent_id: String,
    ) -> zbus::fdo::Result<Vec<DriveFolderRow>> {
        tracing::debug!("received ListDriveFolders request");
        let parent_id = if parent_id.is_empty() {
            "root"
        } else {
            &parent_id
        };
        tokio::time::timeout(
            Duration::from_secs(27),
            self.engine.list_child_folders(parent_id),
        )
        .await
        .map_err(|_| zbus::fdo::Error::Timeout("listing Drive folders exceeded 27 seconds".into()))?
        .map_err(|e| match e {
            SyncError::NotAuthenticated | SyncError::ReauthenticationRequired => {
                zbus::fdo::Error::AccessDenied(format!("Cannot browse Google Drive: {e}"))
            }
            SyncError::Timeout { .. } => zbus::fdo::Error::Timeout(e.to_string()),
            SyncError::Http(ref error) if error.is_timeout() => {
                zbus::fdo::Error::Timeout(format!("failed to list Drive folders: {e}"))
            }
            _ => zbus::fdo::Error::Failed(format!("failed to list Drive folders: {e}")),
        })
    }

    #[zbus(signal)]
    async fn authentication_changed(
        emitter: &SignalEmitter<'_>,
        authenticated: bool,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn sign_in_failed(emitter: &SignalEmitter<'_>, message: &str) -> zbus::Result<()>;
}

fn parse_uuid(id: &str) -> zbus::fdo::Result<Uuid> {
    Uuid::parse_str(id)
        .map_err(|e| zbus::fdo::Error::Failed(format!("invalid sync folder id {id:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gdrive_sync::state_db::StateDb;

    struct Fixture {
        service: GDriveService,
        root: PathBuf,
    }

    impl Fixture {
        fn new(config: AppConfig) -> Self {
            let root = std::env::temp_dir().join(format!("gdrived-auth-test-{}", Uuid::new_v4()));
            let engine = SyncEngine::new(
                config.clone(),
                StateDb::open(&root.join("state.sqlite")).unwrap(),
                String::new(),
            );
            Self {
                service: GDriveService::new(
                    config,
                    root.join("config.toml"),
                    root.join("token.json"),
                    engine,
                ),
                root,
            }
        }

        fn folder(&self, name: &str) -> SyncFolder {
            SyncFolder::new(name, self.root.join(name), "", "")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            for id in self.service.engine.running_folders() {
                self.service.engine.stop_folder(id);
            }
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }

    fn token() -> auth::StoredToken {
        auth::StoredToken {
            access_token: "fixture-token".into(),
            refresh_token: None,
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn root_inspection_is_bounded_off_lock_and_rejects_stale_candidates() {
        for change_config in [false, true] {
            let fixture = Fixture::new(AppConfig::default());
            let permit = Arc::clone(&fixture.service.root_inspection)
                .acquire_owned()
                .await
                .unwrap();
            let path = fixture.root.join("inspected");
            let request = fixture.service.add_sync_folder(
                "inspected".into(),
                "".into(),
                "".into(),
                path.to_string_lossy().into(),
                "".into(),
                "".into(),
                true,
            );
            tokio::pin!(request);
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut request)
                    .await
                    .is_err()
            );
            let state =
                tokio::time::timeout(Duration::from_millis(100), fixture.service.state.lock())
                    .await
                    .unwrap();
            assert!(state.config.sync_folders.is_empty());
            drop(state);
            if change_config {
                fixture
                    .service
                    .add_sync_folder(
                        "other".into(),
                        "".into(),
                        "".into(),
                        fixture.root.join("other").to_string_lossy().into(),
                        "".into(),
                        "".into(),
                        false,
                    )
                    .await
                    .unwrap();
                drop(permit);
                assert!(matches!(request.await, Err(zbus::fdo::Error::Failed(_))));
                let config = AppConfig::load_from(&fixture.service.config_path).unwrap();
                assert_eq!(config.sync_folders.len(), 1);
                assert_eq!(config.sync_folders[0].display_name, "other");
            } else {
                assert!(matches!(request.await, Err(zbus::fdo::Error::Timeout(_))));
                assert!(!fixture.service.config_path.exists());
                assert!(fixture
                    .service
                    .state
                    .lock()
                    .await
                    .config
                    .sync_folders
                    .is_empty());
                drop(permit);
            }
            assert!(!path.exists());
            assert!(fixture.service.engine.running_folders().is_empty());
        }
    }

    #[tokio::test]
    async fn pending_add_and_enable_cannot_cross_session_invalidation() {
        for enable in [false, true] {
            let fixture = Fixture::new(AppConfig::default());
            let mut folder = fixture.folder("pending");
            folder.enabled = false;
            {
                let mut state = fixture.service.state.lock().await;
                if enable {
                    state.config.sync_folders.push(folder.clone());
                }
                fixture.service.persist(&state.config).unwrap();
            }
            let before = std::fs::read(&fixture.service.config_path).unwrap();
            let permit = Arc::clone(&fixture.service.root_inspection)
                .acquire_owned()
                .await
                .unwrap();
            let pending = async {
                if enable {
                    fixture
                        .service
                        .set_folder_enabled(folder.id.to_string(), true)
                        .await
                } else {
                    fixture
                        .service
                        .add_sync_folder(
                            "pending".into(),
                            "".into(),
                            "".into(),
                            folder.local_path.to_string_lossy().into(),
                            "".into(),
                            "".into(),
                            true,
                        )
                        .await
                        .map(|_| ())
                }
            };
            tokio::pin!(pending);
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut pending)
                    .await
                    .is_err()
            );
            fixture.service.state.lock().await.session_revision += 1;
            drop(permit);
            assert!(matches!(pending.await, Err(zbus::fdo::Error::Failed(_))));
            assert_eq!(std::fs::read(&fixture.service.config_path).unwrap(), before);
            assert!(!folder.local_path.exists());
            assert!(fixture.service.engine.running_folders().is_empty());
            if enable {
                fixture
                    .service
                    .set_folder_enabled(folder.id.to_string(), true)
                    .await
                    .unwrap();
            } else {
                fixture
                    .service
                    .add_sync_folder(
                        "after sign-out".into(),
                        "".into(),
                        "".into(),
                        folder.local_path.to_string_lossy().into(),
                        "".into(),
                        "".into(),
                        true,
                    )
                    .await
                    .unwrap();
            }
            assert_eq!(fixture.service.engine.running_folders().len(), 1);
        }
    }

    #[tokio::test]
    async fn a_new_sign_in_attempt_does_not_invalidate_authenticated_startup() {
        let fixture = Fixture::new(AppConfig::default());
        let folder = fixture.folder("authenticated");
        fixture
            .service
            .engine
            .save_token(token(), &fixture.service.token_path)
            .unwrap();
        fixture
            .service
            .state
            .lock()
            .await
            .config
            .sync_folders
            .push(folder.clone());
        let permit = Arc::clone(&fixture.service.root_inspection)
            .acquire_owned()
            .await
            .unwrap();
        let startup = fixture.service.start_configured_folders(0, false);
        tokio::pin!(startup);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut startup)
                .await
                .is_err()
        );
        fixture.service.state.lock().await.sign_in_generation += 1;
        drop(permit);
        startup.await;
        assert_eq!(fixture.service.engine.running_folders(), vec![folder.id]);
        fixture.service.engine.stop_folder(folder.id);
        assert!(fixture.service.is_authenticated().await.unwrap());
    }

    #[tokio::test]
    async fn delayed_startup_cannot_reverse_a_disable_or_sign_out() {
        for sign_out in [false, true] {
            let fixture = Fixture::new(AppConfig::default());
            let folder = fixture.folder("delayed");
            fixture
                .service
                .state
                .lock()
                .await
                .config
                .sync_folders
                .push(folder.clone());
            let permit = Arc::clone(&fixture.service.root_inspection)
                .acquire_owned()
                .await
                .unwrap();
            let startup = fixture.service.start_configured_folders(0, false);
            tokio::pin!(startup);
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut startup)
                    .await
                    .is_err()
            );
            {
                let mut state = fixture.service.state.lock().await;
                if sign_out {
                    state.session_revision += 1;
                } else {
                    state.config.sync_folders[0].enabled = false;
                }
            }
            drop(permit);
            startup.await;
            assert!(!folder.local_path.exists());
            assert!(fixture.service.engine.running_folders().is_empty());
        }
    }

    #[tokio::test]
    async fn removing_or_disabling_a_conflict_restarts_the_other_blocked_binding() {
        for (remove, duplicate_id) in [(false, false), (true, false), (false, true)] {
            let fixture = Fixture::new(AppConfig::default());
            let first = fixture.folder("first");
            let mut second = fixture.folder("first/nested");
            if duplicate_id {
                second.id = first.id;
            }
            {
                let mut state = fixture.service.state.lock().await;
                state.config.sync_folders = vec![first.clone(), second.clone()];
                fixture.service.engine.start_all(&state.config);
            }
            assert!(fixture.service.engine.running_folders().is_empty());
            if remove {
                fixture
                    .service
                    .remove_sync_folder(second.id.to_string())
                    .await
                    .unwrap();
            } else {
                fixture
                    .service
                    .set_folder_enabled(second.id.to_string(), false)
                    .await
                    .unwrap();
            }
            assert_eq!(fixture.service.engine.running_folders(), vec![first.id]);
            fixture.service.engine.stop_folder(first.id);
        }
    }

    #[tokio::test]
    async fn overlapping_add_and_enable_leave_configuration_and_files_unchanged() {
        let fixture = Fixture::new(AppConfig::default());
        let first = fixture.folder("first");
        let mut paused = fixture.folder("first/nested");
        paused.enabled = false;
        {
            let mut state = fixture.service.state.lock().await;
            state.config.sync_folders = vec![first.clone(), paused.clone()];
            fixture.service.persist(&state.config).unwrap();
        }
        let before = std::fs::read(&fixture.service.config_path).unwrap();
        for path in [&first.local_path, &first.local_path.join("another")] {
            let error = fixture
                .service
                .add_sync_folder(
                    "overlap".into(),
                    "".into(),
                    "".into(),
                    path.to_string_lossy().into(),
                    "".into(),
                    "".into(),
                    true,
                )
                .await
                .unwrap_err();
            assert!(matches!(error, zbus::fdo::Error::InvalidArgs(_)));
        }
        assert!(matches!(
            fixture
                .service
                .set_folder_enabled(paused.id.to_string(), true)
                .await,
            Err(zbus::fdo::Error::InvalidArgs(_))
        ));
        assert_eq!(std::fs::read(&fixture.service.config_path).unwrap(), before);
        assert_eq!(
            fixture.service.state.lock().await.config.sync_folders,
            vec![first.clone(), paused]
        );
        assert!(!first.local_path.exists());
        assert!(fixture.service.engine.running_folders().is_empty());
        fixture
            .service
            .set_folder_enabled(first.id.to_string(), false)
            .await
            .unwrap();
        assert!(!fixture.service.state.lock().await.config.sync_folders[0].enabled);
    }

    #[tokio::test]
    async fn selected_paths_survive_persistence_and_listing_without_changing_drive_ids() {
        let fixture = Fixture::new(AppConfig::default());
        for (index, (drive_id, path, expected_id, expected_path)) in [
            (
                "photos-id",
                "My Drive/Parent/Photos",
                Some("photos-id"),
                Some("My Drive/Parent/Photos"),
            ),
            ("manual-id", "", Some("manual-id"), None),
            ("", "", None, Some("My Drive")),
            ("root", "ignored", None, Some("My Drive")),
        ]
        .into_iter()
        .enumerate()
        {
            let id = fixture
                .service
                .add_sync_folder(
                    "Display name".into(),
                    drive_id.into(),
                    path.into(),
                    fixture
                        .root
                        .join(format!("folder-{index}"))
                        .to_string_lossy()
                        .into(),
                    "".into(),
                    "".into(),
                    false,
                )
                .await
                .unwrap();
            let loaded = AppConfig::load_from(&fixture.service.config_path).unwrap();
            let folder = &loaded.sync_folders[index];
            assert_eq!(folder.id.to_string(), id);
            assert_eq!(folder.display_name, "Display name");
            assert_eq!(folder.drive_folder_id.as_deref(), expected_id);
            assert_eq!(folder.drive_folder_path.as_deref(), expected_path);
        }
        let reloaded = Fixture::new(AppConfig::load_from(&fixture.service.config_path).unwrap());
        let rows = reloaded.service.list_sync_folders().await;
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].2, "photos-id");
        assert_eq!(rows[0].3, "My Drive/Parent/Photos");
        assert_eq!(rows[1].2, "manual-id");
        assert_eq!(rows[1].3, "");
        assert_eq!(rows[2].2, "");
        assert_eq!(rows[2].3, "My Drive");
        assert_eq!(rows[3].2, "");
        assert_eq!(rows[3].3, "My Drive");
    }

    #[tokio::test]
    async fn sign_in_uses_live_config_and_later_mutations_cannot_be_undone() {
        let fixture = Fixture::new(AppConfig::default());
        let disabled = fixture.folder("disabled");
        let removed = fixture.folder("removed");
        {
            let mut state = fixture.service.state.lock().await;
            state.config.sync_folders = vec![disabled.clone(), removed.clone()];
            state.sign_in_generation = 1;
        }
        fixture
            .service
            .set_folder_enabled(disabled.id.to_string(), false)
            .await
            .unwrap();
        fixture
            .service
            .remove_sync_folder(removed.id.to_string())
            .await
            .unwrap();
        let added = fixture
            .service
            .add_sync_folder(
                "added".into(),
                "".into(),
                "".into(),
                fixture.root.join("added").to_string_lossy().into(),
                "".into(),
                "".into(),
                true,
            )
            .await
            .unwrap();
        // Stop the task started by Add so the completion must start it from live config.
        fixture
            .service
            .engine
            .stop_folder(parse_uuid(&added).unwrap());
        {
            let mut state = fixture.service.state.lock().await;
            assert!(state
                .complete_sign_in(
                    1,
                    token(),
                    &fixture.service.token_path,
                    &fixture.service.engine
                )
                .unwrap());
        }
        fixture.service.start_configured_folders(1, false).await;
        assert_eq!(
            fixture.service.engine.running_folders(),
            vec![parse_uuid(&added).unwrap()]
        );
        assert!(fixture.service.is_authenticated().await.unwrap());
        fixture
            .service
            .set_folder_enabled(added.clone(), false)
            .await
            .unwrap();
        assert!(fixture.service.engine.running_folders().is_empty());
        fixture.service.remove_sync_folder(added).await.unwrap();
        assert_eq!(fixture.service.list_sync_folders().await.len(), 1);
    }

    #[tokio::test]
    async fn cancelled_sign_in_cannot_persist_tokens_or_restart_folders() {
        let fixture = Fixture::new(AppConfig::default());
        let task = tokio::spawn(std::future::pending::<()>());
        let mut state = fixture.service.state.lock().await;
        state.config.sync_folders.push(fixture.folder("cancelled"));
        state.sign_in_generation = 1;
        state.sign_in = Some(task);
        let cancelled = state.cancel_sign_in().unwrap();
        assert!(state.sign_in_busy());
        assert!(!state
            .complete_sign_in(
                1,
                token(),
                &fixture.service.token_path,
                &fixture.service.engine
            )
            .unwrap());
        assert!(!fixture.service.token_path.exists());
        assert!(fixture.service.engine.running_folders().is_empty());
        drop(state);
        fixture
            .service
            .finish_sign_in_cancellation(Some(cancelled), SIGN_IN_CANCEL_TIMEOUT)
            .await
            .unwrap();
        assert!(!fixture.service.state.lock().await.sign_in_busy());
    }

    #[tokio::test]
    async fn cancellation_releases_listener_before_allowing_another_sign_in() {
        let fixture = Fixture::new(AppConfig::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            std::future::pending::<()>().await;
            drop(listener);
        });
        let cancelled = {
            let mut state = fixture.service.state.lock().await;
            state.sign_in = Some(task);
            let cancelled = state.cancel_sign_in();
            assert!(state.sign_in_busy());
            assert!(std::net::TcpListener::bind(address).is_err());
            cancelled
        };
        fixture
            .service
            .finish_sign_in_cancellation(cancelled, SIGN_IN_CANCEL_TIMEOUT)
            .await
            .unwrap();
        assert!(!fixture.service.state.lock().await.sign_in_busy());
        let _replacement = std::net::TcpListener::bind(address).unwrap();
    }

    #[tokio::test]
    async fn cancellation_timeout_retains_gate_until_task_finishes() {
        let fixture = Fixture::new(AppConfig::default());
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let task = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
        });
        ready.await.unwrap();
        let cancelled = {
            let mut state = fixture.service.state.lock().await;
            state.sign_in = Some(task);
            state.cancel_sign_in()
        };
        let error = fixture
            .service
            .finish_sign_in_cancellation(cancelled, Duration::from_millis(25))
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("cancellation is still in progress"));
        let task = {
            let mut state = fixture.service.state.lock().await;
            assert!(state.sign_in_busy());
            state.sign_in.take().unwrap()
        };
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(!fixture.service.state.lock().await.sign_in_busy());
    }

    #[tokio::test]
    async fn failed_token_persistence_does_not_start_sync() {
        let fixture = Fixture::new(AppConfig::default());
        let mut state = fixture.service.state.lock().await;
        state.config.sync_folders.push(fixture.folder("failed"));
        let error = state
            .complete_sign_in(0, token(), &fixture.root, &fixture.service.engine)
            .unwrap_err();
        assert!(matches!(error, SyncError::Io(_)));
        assert!(std::error::Error::source(&error).is_some());
        assert!(fixture.service.engine.running_folders().is_empty());
    }

    #[tokio::test]
    async fn failed_config_persistence_leaves_config_and_engine_unchanged() {
        let mut fixture = Fixture::new(AppConfig::default());
        let mut folder = fixture.folder("unchanged");
        folder.enabled = false;
        fixture
            .service
            .state
            .lock()
            .await
            .config
            .sync_folders
            .push(folder.clone());
        fixture.service.config_path = fixture.root.clone();
        assert!(fixture
            .service
            .set_folder_enabled(folder.id.to_string(), true)
            .await
            .is_err());
        assert!(fixture
            .service
            .remove_sync_folder(folder.id.to_string())
            .await
            .is_err());
        assert!(fixture
            .service
            .add_sync_folder(
                "failed".into(),
                "".into(),
                "".into(),
                fixture.root.join("failed").to_string_lossy().into(),
                "".into(),
                "".into(),
                true,
            )
            .await
            .is_err());
        assert_eq!(
            fixture.service.state.lock().await.config.sync_folders,
            vec![folder]
        );
        assert!(fixture.service.engine.running_folders().is_empty());
    }

    #[tokio::test]
    #[ignore = "requires isolated bus: dbus-run-session -- env GDRIVE_UI_TEST_BUS=1 cargo test -p gdrived -- --ignored"]
    async fn authentication_signals_cover_cancellation_and_current_attempt_failures() {
        use zbus::export::futures_core::Stream;

        assert_eq!(std::env::var("GDRIVE_UI_TEST_BUS").as_deref(), Ok("1"));
        tokio::time::timeout(Duration::from_secs(5), async {
            let fixture = Fixture::new(AppConfig::default());
            auth::save_token(&fixture.service.token_path, &token()).unwrap();
            let task = tokio::spawn(std::future::pending::<()>());
            {
                let mut state = fixture.service.state.lock().await;
                state.sign_in_generation = 1;
                state.sign_in = Some(task);
            }
            let connection = zbus::connection::Builder::session()
                .method_timeout(Duration::from_secs(2))
                .name(gdrive_common::dbus_api::BUS_NAME)
                .serve_at(
                    gdrive_common::dbus_api::OBJECT_PATH,
                    fixture.service.clone(),
                )
                .build()
                .await
                .unwrap();
            let proxy = gdrive_common::dbus_api::GDrive1Proxy::new(&connection)
                .await
                .unwrap();
            let id = proxy
                .add_sync_folder(
                    "Photos",
                    "photos-id",
                    "My Drive/Parent/Photos",
                    "/fixture/photos",
                    "",
                    "",
                    false,
                )
                .await
                .unwrap();
            let rows = proxy.list_sync_folders().await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].0, id);
            assert_eq!(rows[0].2, "photos-id");
            assert_eq!(rows[0].3, "My Drive/Parent/Photos");
            assert!(proxy.sync_status().await.unwrap().folders.is_empty());
            let mut status_signals = proxy.receive_sync_status_changed().await.unwrap();
            let forwarder = tokio::spawn(GDriveService::forward_status(
                connection.clone(),
                fixture.service.engine.subscribe_status(),
            ));
            let invalid_path = fixture.root.join("not-a-folder");
            std::fs::write(&invalid_path, b"fixture").unwrap();
            let folder = SyncFolder::new("fixture", invalid_path, "", "");
            fixture.service.engine.start_folder(folder.clone());
            loop {
                let signal = std::future::poll_fn(|cx| {
                    std::pin::Pin::new(&mut status_signals).poll_next(cx)
                })
                .await
                .unwrap();
                let snapshot = proxy.sync_status().await.unwrap();
                assert!(snapshot.revision >= signal.args().unwrap().revision);
                if !snapshot.folders[0].errors.is_empty() {
                    assert!(!snapshot.folders[0].syncing);
                    break;
                }
            }
            fixture.service.engine.stop_folder(folder.id);
            assert!(proxy.sync_status().await.unwrap().folders.is_empty());
            forwarder.abort();
            assert!(forwarder.await.unwrap_err().is_cancelled());
            let mut signals = proxy.receive_authentication_changed().await.unwrap();
            assert!(proxy.is_authenticated().await.unwrap());
            proxy.sign_out().await.unwrap();
            assert!(fixture.service.state.lock().await.session_revision > 0);
            let signal = std::future::poll_fn(|cx| std::pin::Pin::new(&mut signals).poll_next(cx))
                .await
                .unwrap();
            assert!(!signal.args().unwrap().authenticated);
            assert!(!proxy.is_authenticated().await.unwrap());
            let mut state = fixture.service.state.lock().await;
            assert!(!state.sign_in_busy());
            assert!(!state
                .complete_sign_in(
                    1,
                    token(),
                    &fixture.service.token_path,
                    &fixture.service.engine
                )
                .unwrap());
            assert!(!fixture.service.token_path.exists());
            let generation = state.sign_in_generation;
            drop(state);
            let interface = connection
                .object_server()
                .interface::<_, GDriveService>(gdrive_common::dbus_api::OBJECT_PATH)
                .await
                .unwrap();
            let emitter = interface.signal_emitter();
            let mut failures = proxy.receive_sign_in_failed().await.unwrap();
            fixture
                .service
                .finish_sign_in(generation - 1, Err(SyncError::OAuthDeclined), emitter)
                .await;
            assert!(tokio::time::timeout(
                Duration::from_millis(25),
                std::future::poll_fn(|cx| std::pin::Pin::new(&mut failures).poll_next(cx))
            )
            .await
            .is_err());
            fixture
                .service
                .finish_sign_in(generation, Err(SyncError::OAuthDeclined), emitter)
                .await;
            let failure =
                std::future::poll_fn(|cx| std::pin::Pin::new(&mut failures).poll_next(cx))
                    .await
                    .unwrap();
            assert!(failure.args().unwrap().message.contains("declined"));
            std::fs::create_dir(&fixture.service.token_path).unwrap();
            fixture
                .service
                .finish_sign_in(generation, Ok(token()), emitter)
                .await;
            let failure =
                std::future::poll_fn(|cx| std::pin::Pin::new(&mut failures).poll_next(cx))
                    .await
                    .unwrap();
            let args = failure.args().unwrap();
            assert!(args.message.starts_with("Sign-in failed."));
            assert!(!args.message.contains("fixture-token"));
            assert!(!fixture.service.state.lock().await.sign_in_busy());
        })
        .await
        .expect("private-bus authentication checks must complete within five seconds");
    }
}
