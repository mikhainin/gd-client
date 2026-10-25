//! D-Bus service exposed by the daemon so the `gdrive-ui` Qt application (or
//! any other client, e.g. `busctl`) can inspect and control synchronisation
//! without talking to the engine/config directly.
//!
//! Bus name: `org.gclient.GDrive1`, object path: `/org/gclient/GDrive1`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gdrive_common::dbus_api::{DriveFolderRow, SyncFolderRow};
use gdrive_common::{AppConfig, SyncFolder};
use gdrive_sync::auth::{self, OAuthConfig};
use gdrive_sync::{SyncEngine, SyncError};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;
use zbus::interface;
use zbus::object_server::SignalEmitter;

const SIGNAL_TIMEOUT: Duration = Duration::from_secs(2);
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const SIGN_IN_CANCEL_TIMEOUT: Duration = Duration::from_secs(2);

struct ServiceState {
    config: AppConfig,
    sign_in_generation: u64,
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
    ) -> zbus::fdo::Result<bool> {
        if self.sign_in_generation != generation {
            return Ok(false);
        }
        self.sign_in = None;
        auth::save_token(token_path, &token)
            .map_err(|e| zbus::fdo::Error::Failed(format!("failed to save token: {e}")))?;
        engine.set_access_token(token.access_token);
        engine.start_all(&self.config);
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
}

impl GDriveService {
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
                sign_in: None,
                cancelling_sign_in: false,
            })),
            config_path,
            token_path,
            engine,
        }
    }

    fn persist(&self, config: &AppConfig) -> zbus::fdo::Result<()> {
        config
            .save_to(&self.config_path)
            .map_err(|e| zbus::fdo::Error::Failed(format!("failed to save configuration: {e}")))
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
        match tokio::time::timeout(
            SIGNAL_TIMEOUT,
            Self::authentication_changed(emitter, authenticated),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!("failed to emit AuthenticationChanged: {error}"),
            Err(_) => tracing::warn!("AuthenticationChanged emission timed out"),
        }
    }
}

/// Implements the `org.gclient.GDrive1` interface defined in
/// `gdrive_common::dbus_api` (see [`gdrive_common::dbus_api::GDrive1`] for
/// the client-side proxy consumed by `gdrive-ui`). The method
/// signatures here must stay in sync with that trait.
#[interface(name = "org.gclient.GDrive1")]
impl GDriveService {
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
        folder.drive_folder_id = if drive_folder_id.is_empty() {
            None
        } else {
            Some(drive_folder_id)
        };
        folder.enabled = enabled;
        let id = folder.id;

        let mut state = self.state.lock().await;
        let mut config = state.config.clone();
        config.sync_folders.push(folder.clone());
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
        self.engine.stop_folder(folder_id);
        Ok(())
    }

    /// Enables or disables a sync folder, starting/stopping its background
    /// synchronisation task accordingly.
    async fn set_folder_enabled(&self, id: String, enabled: bool) -> zbus::fdo::Result<()> {
        let folder_id = parse_uuid(&id)?;

        let mut state = self.state.lock().await;
        let mut config = state.config.clone();
        let folder = config
            .sync_folders
            .iter_mut()
            .find(|f| f.id == folder_id)
            .ok_or_else(|| zbus::fdo::Error::Failed(format!("no such sync folder: {id}")))?;
        folder.enabled = enabled;
        let folder = folder.clone();
        self.persist(&config)?;
        state.config = config;

        if enabled {
            self.engine.start_folder(folder);
        } else {
            self.engine.stop_folder(folder_id);
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
    /// [`Self::authentication_changed`].
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
            .map_err(|_| SyncError::Timeout("sign-in exceeded 10 minutes"))
            .and_then(|result| result);

            let mut state = service.state.lock().await;
            if state.sign_in_generation != generation {
                return;
            }
            state.sign_in = None;
            match result {
                Ok(token) => {
                    match state.complete_sign_in(
                        generation,
                        token,
                        &service.token_path,
                        &service.engine,
                    ) {
                        Ok(true) => {
                            tracing::info!("sign-in succeeded");
                            Self::notify_authentication(&emitter, true).await;
                        }
                        Ok(false) => {}
                        Err(error) => tracing::error!("sign-in failed: {error}"),
                    }
                }
                Err(err) => tracing::error!("sign-in failed: {err}"),
            }
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
            let task = state.cancel_sign_in();
            let removal = if self.token_path.exists() {
                std::fs::remove_file(&self.token_path).map_err(|e| {
                    zbus::fdo::Error::Failed(format!("failed to remove cached token: {e}"))
                })
            } else {
                Ok(())
            };

            for folder_id in self.engine.running_folders() {
                self.engine.stop_folder(folder_id);
            }
            self.engine.set_access_token(String::new());
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
            SyncError::NotAuthenticated => zbus::fdo::Error::AccessDenied(
                "Sign in to Google Drive before browsing folders".into(),
            ),
            SyncError::Timeout(_) => zbus::fdo::Error::Timeout(e.to_string()),
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
        assert!(error.to_string().contains("failed to save token"));
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
    async fn sign_out_emits_signal_and_invalidates_pending_sign_in() {
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
                .unwrap()
                .method_timeout(Duration::from_secs(2))
                .name(gdrive_common::dbus_api::BUS_NAME)
                .unwrap()
                .serve_at(
                    gdrive_common::dbus_api::OBJECT_PATH,
                    fixture.service.clone(),
                )
                .unwrap()
                .build()
                .await
                .unwrap();
            let proxy = gdrive_common::dbus_api::GDrive1Proxy::new(&connection)
                .await
                .unwrap();
            let mut signals = proxy.receive_authentication_changed().await.unwrap();
            assert!(proxy.is_authenticated().await.unwrap());
            proxy.sign_out().await.unwrap();
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
        })
        .await
        .expect("private-bus sign-out must complete within five seconds");
    }
}
