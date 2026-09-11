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
use gdrive_sync::auth::{self, OAuthConfig};
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
    // have no way to discover *why* nothing is syncing. Sync folders simply
    // won't be started until `gdrived` is restarted after authenticating -
    // see the module doc on `obtain_access_token`.
    let access_token = match obtain_access_token().await {
        Ok(token) => Some(token),
        Err(err) => {
            tracing::warn!(
                "not authenticated with Google Drive yet ({err}); the D-Bus service will still \
                 start, but no sync folders will run until gdrived is restarted after authenticating"
            );
            None
        }
    };

    let state_db_path = gdrive_common::paths::sync_state_db_file()?;
    let state_db = StateDb::open(&state_db_path)?;

    let authenticated = access_token.is_some();
    let engine = SyncEngine::new(config.clone(), state_db, access_token.unwrap_or_default());
    if authenticated {
        engine.start_all(&config);
    }

    let service = dbus_service::GDriveService::new(config, config_path, engine.clone());

    let _connection = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, service)?
        .build()
        .await?;
    tracing::info!(bus_name = BUS_NAME, object_path = OBJECT_PATH, "D-Bus service ready");

    // Log sync events for now; a future revision can forward these as D-Bus
    // signals (see dbus_service.rs) once the UI needs live progress updates
    // rather than polling ListSyncFolders.
    let mut events = engine.subscribe();
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            tracing::info!(?event, "sync event");
        }
    });

    wait_for_shutdown_signal().await;
    tracing::info!("shutting down");
    Ok(())
}

/// Returns a cached OAuth access token if one is available, otherwise runs
/// the interactive authentication flow (requires `GDRIVE_CLIENT_ID` /
/// `GDRIVE_CLIENT_SECRET` to be set - see `gdrive_sync::auth`).
///
/// Token refresh is not yet implemented: once Google Cloud OAuth client
/// credentials are wired up, this should check `expires_at` and use the
/// refresh token instead of always trusting the cached access token.
async fn obtain_access_token() -> anyhow::Result<String> {
    let token_path = auth::default_token_path()?;

    if let Some(token) = auth::load_token(&token_path)? {
        return Ok(token.access_token);
    }

    let oauth_config = OAuthConfig::from_env().ok_or_else(|| {
        anyhow::anyhow!(
            "not authenticated with Google Drive yet, and GDRIVE_CLIENT_ID/GDRIVE_CLIENT_SECRET \
             are not set - cannot start the interactive OAuth flow"
        )
    })?;

    let token = auth::authenticate(&oauth_config, &token_path, |url| {
        tracing::info!("open this URL in a browser to authorize gdrived: {url}");
    })
    .await?;

    Ok(token.access_token)
}

async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
}
