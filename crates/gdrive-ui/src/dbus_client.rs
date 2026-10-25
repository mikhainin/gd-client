//! Bounded asynchronous D-Bus requests, executed away from the Qt event loop.

use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

use gdrive_common::dbus_api::{DriveFolderRow, GDrive1Proxy, SyncFolderRow, SyncStatus};
use tokio::runtime::Runtime;
use tokio::task::AbortHandle;
use zbus::export::futures_core::Stream;

pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
pub const BROWSE_TIMEOUT: Duration = Duration::from_secs(30);

pub enum Request {
    Refresh,
    AddFolder {
        display_name: String,
        drive_folder_id: String,
        drive_folder_path: String,
        local_path: String,
        owner_user: String,
        owner_group: String,
        enabled: bool,
    },
    RemoveFolder(String),
    SetFolderEnabled(String, bool),
    SignIn,
    SignOut,
    ListDriveFolders(String),
    DarkMode,
}

#[derive(Debug)]
pub enum Response {
    Snapshot(Vec<SyncFolderRow>, Result<bool, String>),
    Done,
    DriveFolders(Vec<DriveFolderRow>),
    DarkMode(bool),
}

pub enum AuthenticationEvent {
    Ready,
    Changed(bool),
}

#[derive(Debug)]
pub enum SyncStatusEvent {
    Snapshot(SyncStatus),
    Unavailable(String),
}

pub async fn watch_sync_status(notify: impl Fn(SyncStatusEvent) + Send + Sync + 'static) {
    loop {
        if let Err(error) = sync_status_session(&notify).await {
            tracing::warn!("sync status unavailable: {error}");
            notify(SyncStatusEvent::Unavailable(error));
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn sync_status_session(notify: &(impl Fn(SyncStatusEvent) + Sync)) -> Result<(), String> {
    let connection = bounded(CONTROL_TIMEOUT, connect(CONTROL_TIMEOUT)).await?;
    let bus = bounded(CONTROL_TIMEOUT, zbus::fdo::DBusProxy::new(&connection)).await?;
    let mut owners = bounded(
        CONTROL_TIMEOUT,
        bus.receive_name_owner_changed_with_args(&[(0, gdrive_common::dbus_api::BUS_NAME)]),
    )
    .await?;
    let owner = bounded(CONTROL_TIMEOUT, async {
        bus.get_name_owner(
            gdrive_common::dbus_api::BUS_NAME
                .try_into()
                .expect("valid bus name"),
        )
        .await
        .map_err(zbus::Error::from)
    })
    .await?;
    // Bind to this unique owner: a restarted daemon's revision starts over.
    let proxy = bounded(
        CONTROL_TIMEOUT,
        GDrive1Proxy::builder(&connection)
            .destination(owner.as_str())
            .build(),
    )
    .await?;
    let mut signals = bounded(CONTROL_TIMEOUT, proxy.receive_sync_status_changed()).await?;
    let mut poll = tokio::time::interval(Duration::from_secs(3));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    poll.tick().await;
    let mut revision = None;
    loop {
        let snapshot = tokio::select! {
            biased;
            _ = std::future::poll_fn(|cx| std::pin::Pin::new(&mut owners).poll_next(cx)) =>
                return Err("daemon owner changed; reconnecting".into()),
            result = bounded(CONTROL_TIMEOUT, proxy.sync_status()) => result?,
        };
        if revision.is_none_or(|last| snapshot.revision > last) {
            revision = Some(snapshot.revision);
            notify(SyncStatusEvent::Snapshot(snapshot));
        }
        tokio::select! {
            biased;
            _ = std::future::poll_fn(|cx| std::pin::Pin::new(&mut owners).poll_next(cx)) =>
                return Err("daemon owner changed; reconnecting".into()),
            signal = std::future::poll_fn(|cx| std::pin::Pin::new(&mut signals).poll_next(cx)) => {
                let signal = signal.ok_or("sync status signal stream closed")?;
                signal.args().map_err(|error| format!("invalid SyncStatusChanged signal: {error}"))?;
            }
            _ = poll.tick() => {}
        }
    }
}

pub async fn watch_authentication(notify: impl Fn(AuthenticationEvent) + Send + Sync + 'static) {
    loop {
        if let Err(error) = authentication_session(&notify).await {
            tracing::warn!("authentication subscription interrupted: {error}");
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
}

async fn authentication_session(
    notify: &(impl Fn(AuthenticationEvent) + Sync),
) -> Result<(), String> {
    let connection = bounded(CONTROL_TIMEOUT, async {
        zbus::connection::Builder::session()
            .method_timeout(CONTROL_TIMEOUT)
            .build()
            .await
    })
    .await?;
    let proxy = bounded(CONTROL_TIMEOUT, GDrive1Proxy::new(&connection)).await?;
    let bus = bounded(CONTROL_TIMEOUT, zbus::fdo::DBusProxy::new(&connection)).await?;
    let mut owners = bounded(
        CONTROL_TIMEOUT,
        bus.receive_name_owner_changed_with_args(&[(0, gdrive_common::dbus_api::BUS_NAME)]),
    )
    .await?;
    let mut signals = bounded(CONTROL_TIMEOUT, proxy.receive_authentication_changed()).await?;
    notify(AuthenticationEvent::Ready);

    loop {
        tokio::select! {
            signal = std::future::poll_fn(|cx| std::pin::Pin::new(&mut signals).poll_next(cx)) => {
                let signal = signal.ok_or("authentication signal stream closed")?;
                let args = signal.args()
                    .map_err(|error| format!("invalid AuthenticationChanged signal: {error}"))?;
                notify(AuthenticationEvent::Changed(args.authenticated));
            }
            owner = std::future::poll_fn(|cx| std::pin::Pin::new(&mut owners).poll_next(cx)) => {
                owner.ok_or("daemon owner stream closed")?;
                return Ok(());
            }
        }
    }
}

pub fn spawn(future: impl Future<Output = ()> + Send + 'static) -> Result<AbortHandle, String> {
    static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();
    let runtime = RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .map_err(|e| format!("could not start the D-Bus worker: {e}"))
        })
        .as_ref()
        .map_err(Clone::clone)?;
    Ok(runtime.spawn(future).abort_handle())
}

/// Owns at most one task; generations also reject completions already queued to Qt.
#[derive(Default)]
pub struct PendingRequest {
    generation: u64,
    handle: Option<AbortHandle>,
}

impl PendingRequest {
    pub fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }

    pub fn begin(&mut self) -> u64 {
        self.cancel();
        self.generation
    }

    pub fn attach(&mut self, handle: AbortHandle) {
        self.handle = Some(handle);
    }

    pub fn finish(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.handle = None;
        true
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub async fn execute(request: Request) -> Result<Response, String> {
    let deadline = if matches!(request, Request::ListDriveFolders(_)) {
        BROWSE_TIMEOUT
    } else {
        CONTROL_TIMEOUT
    };
    execute_with_timeout(request, deadline).await
}

async fn execute_with_timeout(request: Request, deadline: Duration) -> Result<Response, String> {
    if matches!(request, Request::Refresh) {
        let end = tokio::time::Instant::now() + deadline;
        let connection = bounded_until(end, deadline, connect(deadline)).await?;
        let proxy = bounded_until(end, deadline, GDrive1Proxy::new(&connection)).await?;
        let rows = bounded_until(end, deadline, proxy.list_sync_folders()).await?;
        let authentication = bounded_until(end, deadline, proxy.is_authenticated()).await;
        return Ok(Response::Snapshot(rows, authentication));
    }
    bounded(deadline, call(request, deadline)).await
}

async fn bounded<T>(
    deadline: Duration,
    future: impl Future<Output = zbus::Result<T>>,
) -> Result<T, String> {
    bounded_until(tokio::time::Instant::now() + deadline, deadline, future).await
}

async fn bounded_until<T>(
    end: tokio::time::Instant,
    deadline: Duration,
    future: impl Future<Output = zbus::Result<T>>,
) -> Result<T, String> {
    tokio::time::timeout_at(end, future)
        .await
        .map_err(|_| {
            format!(
                "gdrived request timed out after {} seconds. Check the daemon and connection, then retry.",
                deadline.as_secs()
            )
        })?
        .map_err(|e| format!("D-Bus request failed: {e}"))
}

async fn connect(deadline: Duration) -> zbus::Result<zbus::Connection> {
    zbus::connection::Builder::session()
        .method_timeout(deadline)
        .build()
        .await
}

async fn call(request: Request, deadline: Duration) -> zbus::Result<Response> {
    // The outer deadline includes connecting, authentication, activation and all calls.
    let connection = connect(deadline).await?;

    if matches!(request, Request::DarkMode) {
        let reply = connection
            .call_method(
                Some("org.freedesktop.portal.Desktop"),
                "/org/freedesktop/portal/desktop",
                Some("org.freedesktop.portal.Settings"),
                "Read",
                &("org.freedesktop.appearance", "color-scheme"),
            )
            .await?;
        let value: zbus::OwnedVariant = reply.body().deserialize()?;
        let color_scheme: u32 = zbus::Variant::from(value).downcast()?;
        return Ok(Response::DarkMode(color_scheme == 1));
    }

    let proxy = GDrive1Proxy::new(&connection).await?;
    match request {
        Request::Refresh => unreachable!("refresh handles partial results separately"),
        Request::AddFolder {
            display_name,
            drive_folder_id,
            drive_folder_path,
            local_path,
            owner_user,
            owner_group,
            enabled,
        } => {
            proxy
                .add_sync_folder(
                    &display_name,
                    &drive_folder_id,
                    &drive_folder_path,
                    &local_path,
                    &owner_user,
                    &owner_group,
                    enabled,
                )
                .await?;
            Ok(Response::Done)
        }
        Request::RemoveFolder(id) => {
            proxy.remove_sync_folder(&id).await?;
            Ok(Response::Done)
        }
        Request::SetFolderEnabled(id, enabled) => {
            proxy.set_folder_enabled(&id, enabled).await?;
            Ok(Response::Done)
        }
        Request::SignIn => {
            proxy.sign_in().await?;
            Ok(Response::Done)
        }
        Request::SignOut => {
            proxy.sign_out().await?;
            Ok(Response::Done)
        }
        Request::ListDriveFolders(parent) => Ok(Response::DriveFolders(
            proxy.list_drive_folders(&parent).await?,
        )),
        Request::DarkMode => unreachable!("handled before constructing the daemon proxy"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::sync::Arc;

    #[test]
    fn cancellation_and_new_requests_reject_queued_completions() {
        let mut pending = PendingRequest::default();
        let first = pending.begin();
        pending.cancel();
        assert!(!pending.finish(first));
        let second = pending.begin();
        let third = pending.begin();
        assert!(!pending.finish(second));
        assert!(pending.finish(third));
    }

    #[tokio::test]
    async fn cancellation_aborts_work_instead_of_accumulating_tasks() {
        let mut pending = PendingRequest::default();
        pending.begin();
        let task = tokio::spawn(std::future::pending::<()>());
        pending.attach(task.abort_handle());
        pending.cancel();
        assert!(task.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn deadline_includes_a_stalled_connection_handshake() {
        let (client, _stalled_server) = std::os::unix::net::UnixStream::pair().unwrap();
        let start = std::time::Instant::now();
        let error = bounded(
            Duration::from_millis(50),
            zbus::connection::Builder::unix_stream(client).build(),
        )
        .await
        .unwrap_err();
        assert!(error.contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    struct SlowDaemon(Arc<AtomicBool>, Arc<AtomicU8>);

    struct PortalSettings(Arc<AtomicU8>);

    #[zbus::interface(name = "org.freedesktop.portal.Settings")]
    impl PortalSettings {
        async fn read(&self, namespace: &str, key: &str) -> zbus::OwnedVariant {
            assert_eq!(namespace, "org.freedesktop.appearance");
            assert_eq!(key, "color-scheme");
            match self.0.load(Ordering::SeqCst) {
                0 => 1u32.into(),
                1 => 2u32.into(),
                _ => true.into(),
            }
        }
    }

    #[zbus::interface(name = "org.gclient.GDrive1")]
    impl SlowDaemon {
        async fn list_drive_folders(&self, _parent: String) -> Vec<DriveFolderRow> {
            if self.0.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            vec![("projects".to_string(), "Projects".to_string())]
        }

        async fn list_sync_folders(&self) -> Vec<SyncFolderRow> {
            if self.1.load(Ordering::SeqCst) == 0 {
                vec![]
            } else {
                vec![(
                    "folder".into(),
                    "Configured folder".into(),
                    "projects-id".into(),
                    "My Drive/Projects".into(),
                    "/fixture".into(),
                    "".into(),
                    "".into(),
                    false,
                    false,
                )]
            }
        }

        async fn is_authenticated(&self) -> zbus::fdo::Result<bool> {
            match self.1.load(Ordering::SeqCst) {
                1 => Err(zbus::fdo::Error::Failed(
                    "fixture authentication read failed".into(),
                )),
                2 => {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    Ok(true)
                }
                _ => Ok(true),
            }
        }

        async fn remove_sync_folder(&self, _id: String) -> zbus::fdo::Result<()> {
            Err(zbus::fdo::Error::Failed("fixture mutation failed".into()))
        }

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
        ) -> String {
            assert_eq!(display_name, "Projects");
            assert_eq!(drive_folder_id, "projects-id");
            assert_eq!(drive_folder_path, "My Drive/Projects");
            assert_eq!(local_path, "/fixture");
            assert_eq!(owner_user, "user");
            assert_eq!(owner_group, "group");
            assert!(!enabled);
            "folder".into()
        }

        #[zbus(signal)]
        async fn authentication_changed(
            emitter: &zbus::object_server::SignalEmitter<'_>,
            authenticated: bool,
        ) -> zbus::Result<()>;
    }

    #[tokio::test]
    #[ignore = "requires isolated bus: dbus-run-session -- env GDRIVE_UI_TEST_BUS=1 cargo test -p gdrive-ui -- --ignored"]
    async fn private_bus_timeout_does_not_prevent_other_requests_or_recovery() {
        assert_eq!(std::env::var("GDRIVE_UI_TEST_BUS").as_deref(), Ok("1"));
        let stall = Arc::new(AtomicBool::new(true));
        let auth_mode = Arc::new(AtomicU8::new(0));
        let color_mode = Arc::new(AtomicU8::new(0));
        let _service = bounded(
            CONTROL_TIMEOUT,
            zbus::connection::Builder::session()
                .name(gdrive_common::dbus_api::BUS_NAME)
                .name("org.freedesktop.portal.Desktop")
                .serve_at(
                    gdrive_common::dbus_api::OBJECT_PATH,
                    SlowDaemon(stall.clone(), auth_mode.clone()),
                )
                .serve_at(
                    "/org/freedesktop/portal/desktop",
                    PortalSettings(color_mode.clone()),
                )
                .build(),
        )
        .await
        .unwrap();
        let unavailable = sync_status_session(&|_| panic!("older daemon must not report idle"))
            .await
            .unwrap_err();
        assert!(unavailable.contains("UnknownMethod"), "{unavailable}");
        let start = std::time::Instant::now();
        let (browse, refresh) = tokio::join!(
            execute_with_timeout(
                Request::ListDriveFolders("root".into()),
                Duration::from_millis(100)
            ),
            execute(Request::Refresh)
        );
        assert!(browse.unwrap_err().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(matches!(refresh, Ok(Response::Snapshot(rows, Ok(true))) if rows.is_empty()));
        assert!(matches!(
            execute(Request::Refresh).await,
            Ok(Response::Snapshot(_, Ok(true)))
        ));
        stall.store(false, Ordering::SeqCst);
        assert!(matches!(
            execute(Request::ListDriveFolders("root".into())).await,
            Ok(Response::DriveFolders(rows)) if rows == vec![("projects".into(), "Projects".into())]
        ));
        assert!(execute(Request::RemoveFolder("missing".into()))
            .await
            .unwrap_err()
            .contains("fixture mutation failed"));
        assert!(matches!(
            execute(Request::AddFolder {
                display_name: "Projects".into(),
                drive_folder_id: "projects-id".into(),
                drive_folder_path: "My Drive/Projects".into(),
                local_path: "/fixture".into(),
                owner_user: "user".into(),
                owner_group: "group".into(),
                enabled: false,
            })
            .await,
            Ok(Response::Done)
        ));

        assert!(matches!(
            execute(Request::DarkMode).await,
            Ok(Response::DarkMode(true))
        ));
        color_mode.store(1, Ordering::SeqCst);
        assert!(matches!(
            execute(Request::DarkMode).await,
            Ok(Response::DarkMode(false))
        ));
        color_mode.store(2, Ordering::SeqCst);
        assert!(execute(Request::DarkMode).await.is_err());

        for mode in [1, 2] {
            auth_mode.store(mode, Ordering::SeqCst);
            let response = execute_with_timeout(Request::Refresh, Duration::from_millis(100))
                .await
                .unwrap();
            let Response::Snapshot(rows, Err(error)) = response else {
                panic!("authentication failure must retain configured folders");
            };
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].0, "folder");
            assert_eq!(rows[0].2, "projects-id");
            assert_eq!(rows[0].3, "My Drive/Projects");
            assert!(error.contains(if mode == 1 {
                "authentication read failed"
            } else {
                "timed out"
            }));
        }
        auth_mode.store(0, Ordering::SeqCst);

        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let watcher = tokio::spawn(watch_authentication(move |event| {
            events.send(event).unwrap();
        }));
        assert!(matches!(
            tokio::time::timeout(CONTROL_TIMEOUT, received.recv())
                .await
                .unwrap(),
            Some(AuthenticationEvent::Ready)
        ));
        assert!(matches!(
            execute(Request::Refresh).await,
            Ok(Response::Snapshot(_, Ok(true)))
        ));
        let interface = _service
            .object_server()
            .interface::<_, SlowDaemon>(gdrive_common::dbus_api::OBJECT_PATH)
            .await
            .unwrap();
        bounded(
            CONTROL_TIMEOUT,
            SlowDaemon::authentication_changed(interface.signal_emitter(), false),
        )
        .await
        .unwrap();
        assert!(matches!(
            tokio::time::timeout(CONTROL_TIMEOUT, received.recv())
                .await
                .unwrap(),
            Some(AuthenticationEvent::Changed(false))
        ));

        bounded(
            CONTROL_TIMEOUT,
            _service.release_name(gdrive_common::dbus_api::BUS_NAME),
        )
        .await
        .unwrap();
        assert!(matches!(
            tokio::time::timeout(CONTROL_TIMEOUT, received.recv())
                .await
                .unwrap(),
            Some(AuthenticationEvent::Ready)
        ));
        bounded(
            CONTROL_TIMEOUT,
            _service.request_name(gdrive_common::dbus_api::BUS_NAME),
        )
        .await
        .unwrap();
        assert!(matches!(
            tokio::time::timeout(CONTROL_TIMEOUT, received.recv())
                .await
                .unwrap(),
            Some(AuthenticationEvent::Ready)
        ));
        bounded(
            CONTROL_TIMEOUT,
            SlowDaemon::authentication_changed(interface.signal_emitter(), true),
        )
        .await
        .unwrap();
        assert!(matches!(
            tokio::time::timeout(CONTROL_TIMEOUT, received.recv())
                .await
                .unwrap(),
            Some(AuthenticationEvent::Changed(true))
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), received.recv())
                .await
                .is_err(),
            "reconnection must not leave duplicate signal subscriptions"
        );
        watcher.abort();
        assert!(watcher.await.unwrap_err().is_cancelled());
    }

    struct StatusDaemon {
        status: Arc<std::sync::Mutex<SyncStatus>>,
        stall: Arc<AtomicBool>,
    }

    #[zbus::interface(name = "org.gclient.GDrive1")]
    impl StatusDaemon {
        async fn sync_status(&self) -> SyncStatus {
            if self.stall.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            self.status.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    #[ignore = "requires isolated bus and serialized ignored tests"]
    async fn private_bus_sync_status_recovers_signals_polling_restart_and_timeouts() {
        assert_eq!(std::env::var("GDRIVE_UI_TEST_BUS").as_deref(), Ok("1"));
        let status = Arc::new(std::sync::Mutex::new(SyncStatus {
            revision: 1,
            folders: vec![],
        }));
        let stall = Arc::new(AtomicBool::new(false));
        let service = bounded(
            CONTROL_TIMEOUT,
            zbus::connection::Builder::session()
                .method_timeout(CONTROL_TIMEOUT)
                .name(gdrive_common::dbus_api::BUS_NAME)
                .serve_at(
                    gdrive_common::dbus_api::OBJECT_PATH,
                    StatusDaemon {
                        status: status.clone(),
                        stall: stall.clone(),
                    },
                )
                .build(),
        )
        .await
        .unwrap();
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let watcher = tokio::spawn(watch_sync_status(move |event| {
            let _ = events.send(event);
        }));
        async fn next(
            events: &mut tokio::sync::mpsc::UnboundedReceiver<SyncStatusEvent>,
        ) -> SyncStatusEvent {
            tokio::time::timeout(Duration::from_secs(9), events.recv())
                .await
                .unwrap()
                .unwrap()
        }
        assert!(matches!(
            next(&mut received).await,
            SyncStatusEvent::Snapshot(SyncStatus { revision: 1, .. })
        ));
        {
            let mut value = status.lock().unwrap();
            value.revision = 2;
            value
                .folders
                .push(gdrive_common::dbus_api::FolderSyncStatus {
                    folder_id: "fixture".into(),
                    syncing: true,
                    errors: vec![],
                });
        }
        bounded(
            CONTROL_TIMEOUT,
            service.emit_signal(
                None::<&str>,
                gdrive_common::dbus_api::OBJECT_PATH,
                "org.gclient.GDrive1",
                "SyncStatusChanged",
                &(2u64,),
            ),
        )
        .await
        .unwrap();
        assert!(
            matches!(next(&mut received).await, SyncStatusEvent::Snapshot(value) if value.revision == 2 && value.folders[0].syncing)
        );
        {
            let mut value = status.lock().unwrap();
            value.revision = 3;
            value.folders[0].syncing = false;
            value.folders[0].errors.push("upload failed".into());
        }
        // No signal: periodic snapshots recover a lost notification.
        assert!(
            matches!(next(&mut received).await, SyncStatusEvent::Snapshot(value) if value.revision == 3 && value.folders[0].errors == ["upload failed"])
        );
        status.lock().unwrap().revision = 2;
        bounded(
            CONTROL_TIMEOUT,
            service.emit_signal(
                None::<&str>,
                gdrive_common::dbus_api::OBJECT_PATH,
                "org.gclient.GDrive1",
                "SyncStatusChanged",
                &(2u64,),
            ),
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), received.recv())
                .await
                .is_err(),
            "obsolete snapshot must not replace newer errors"
        );
        bounded(
            CONTROL_TIMEOUT,
            service.release_name(gdrive_common::dbus_api::BUS_NAME),
        )
        .await
        .unwrap();
        assert!(matches!(
            next(&mut received).await,
            SyncStatusEvent::Unavailable(_)
        ));
        status.lock().unwrap().revision = 0;
        let replacement = bounded(
            CONTROL_TIMEOUT,
            zbus::connection::Builder::session()
                .method_timeout(CONTROL_TIMEOUT)
                .name(gdrive_common::dbus_api::BUS_NAME)
                .serve_at(
                    gdrive_common::dbus_api::OBJECT_PATH,
                    StatusDaemon {
                        status: status.clone(),
                        stall: stall.clone(),
                    },
                )
                .build(),
        )
        .await
        .unwrap();
        assert!(
            matches!(next(&mut received).await, SyncStatusEvent::Snapshot(value) if value.revision == 0)
        );
        stall.store(true, Ordering::SeqCst);
        assert!(
            matches!(next(&mut received).await, SyncStatusEvent::Unavailable(error) if error.contains("timed out"))
        );
        stall.store(false, Ordering::SeqCst);
        assert!(
            matches!(next(&mut received).await, SyncStatusEvent::Snapshot(value) if value.revision == 0)
        );
        watcher.abort();
        assert!(watcher.await.unwrap_err().is_cancelled());
        bounded(
            CONTROL_TIMEOUT,
            replacement.release_name(gdrive_common::dbus_api::BUS_NAME),
        )
        .await
        .unwrap();
    }
}
