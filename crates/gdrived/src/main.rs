//! `gdrived`: background service that keeps configured local folders in sync
//! with Google Drive.
//!
//! On startup it loads the persisted configuration and cached OAuth token,
//! starts the sync engine for every enabled folder, and exposes a D-Bus
//! interface (see [`dbus_service`]) on the session bus at
//! `org.gclient.GDrive1` so the `gdrive-ui` application can list/add/remove/
//! toggle sync folders at runtime.

mod dbus_service;

use gdrive_common::dbus_api::{BUS_NAME, OBJECT_PATH};
use gdrive_common::AppConfig;
use gdrive_sync::auth;
use gdrive_sync::state_db::StateDb;
use gdrive_sync::SyncEngine;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config_path = gdrive_common::paths::config_file()?;
    let config = AppConfig::load_from(&config_path)?;
    tracing::info!(folders = config.sync_folders.len(), "loaded configuration");

    // Not being authenticated yet must not prevent the D-Bus service (and
    // therefore the UI) from starting: without it, a first-run user would
    // have no way to discover *why* nothing is syncing. Only a *cached*
    // token is loaded here - if none exists yet, the daemon still starts
    // (with no folders running) and the UI's "Sign in" button drives the
    // interactive OAuth flow via the `SignIn` D-Bus method instead, so
    // startup never blocks on the user completing browser-based consent.
    let access_token = match load_cached_access_token() {
        Ok(Some(token)) => Some(token),
        Ok(None) => {
            tracing::info!(
                "not authenticated with Google Drive yet; sign in via the UI's \"Sign in\" \
                 button (or the SignIn D-Bus method) to start syncing"
            );
            None
        }
        Err(err) => {
            tracing::warn!("failed to load cached OAuth token ({err}); treating as signed out");
            None
        }
    };

    let state_db_path = gdrive_common::paths::sync_state_db_file()?;
    let state_db = StateDb::open(&state_db_path)?;

    let authenticated = access_token.is_some();
    let engine = SyncEngine::new(config.clone(), state_db, access_token.unwrap_or_default());
    let mut events = engine.subscribe();
    let status_changes = engine.subscribe_status();
    if authenticated {
        engine.start_all(&config);
    }

    let service = dbus_service::GDriveService::new(
        config,
        config_path,
        auth::default_token_path()?,
        engine.clone(),
    );

    let connection = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        zbus::connection::Builder::session()
            .method_timeout(std::time::Duration::from_secs(5))
            .name(BUS_NAME)
            .serve_at(OBJECT_PATH, service)
            .build(),
    )
    .await??;
    tracing::info!(
        bus_name = BUS_NAME,
        object_path = OBJECT_PATH,
        "D-Bus service ready"
    );

    tokio::spawn(dbus_service::GDriveService::forward_status(
        connection.clone(),
        status_changes,
    ));
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => tracing::info!(?event, "sync event"),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => tracing::warn!(
                    count,
                    "sync log events skipped; current status remains available"
                ),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    wait_for_shutdown_signal().await;
    tracing::info!("shutting down");
    Ok(())
}

/// Returns a cached OAuth access token if one is available, without ever
/// starting the interactive authentication flow (that's now only triggered
/// on demand via the D-Bus `SignIn` method - see `dbus_service::sign_in`).
///
/// Token refresh is not yet implemented: once Google Cloud OAuth client
/// credentials are wired up, this should check `expires_at` and use the
/// refresh token instead of always trusting the cached access token.
fn load_cached_access_token() -> anyhow::Result<Option<String>> {
    let token_path = auth::default_token_path()?;
    Ok(auth::load_token(&token_path)?.map(|token| token.access_token))
}

async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
}
