//! Error type for the sync engine crate.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error(transparent)]
    Common(#[from] gdrive_common::CommonError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("local filesystem watcher error: {0}")]
    Notify(#[from] notify::Error),

    #[error("SQLite state store error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("Drive API request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Drive API returned an error: {0}")]
    Api(String),

    #[error("not authenticated with Google Drive yet")]
    NotAuthenticated,

    #[error("Google Drive credentials expired or were revoked. Sign out and sign in again.")]
    ReauthenticationRequired,

    #[error("timed out while {operation}")]
    Timeout {
        operation: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("OAuth token refresh transport failed; check the connection and retry")]
    OAuthTransport(#[source] crate::auth::ReqwestOAuthError),

    #[error("OAuth2 error: {0}")]
    OAuth(String),
}
