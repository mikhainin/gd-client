//! Standard filesystem locations used by the service and the UI, following
//! the XDG base directory specification (via the `directories` crate) so
//! behaviour is consistent across Linux and BSD desktops.

use std::path::PathBuf;

use directories::ProjectDirs;

use crate::error::CommonError;

const QUALIFIER: &str = "";
const ORGANIZATION: &str = "g-client";
const APPLICATION: &str = "gdrive-client";

fn project_dirs() -> Result<ProjectDirs, CommonError> {
    ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
        .ok_or(CommonError::NoHomeDirectory)
}

/// Path to the persisted TOML configuration file
/// (`~/.config/gdrive-client/config.toml` on Linux).
pub fn config_file() -> Result<PathBuf, CommonError> {
    Ok(project_dirs()?.config_dir().join("config.toml"))
}

/// Directory for persistent service state: the SQLite sync-state database,
/// OAuth tokens, etc. (`~/.local/share/gdrive-client` on Linux).
pub fn data_dir() -> Result<PathBuf, CommonError> {
    let dir = project_dirs()?.data_dir().to_path_buf();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Path to the SQLite database tracking local<->remote file state.
pub fn sync_state_db_file() -> Result<PathBuf, CommonError> {
    Ok(data_dir()?.join("sync-state.sqlite3"))
}

/// Path to the cached OAuth2 token for the Drive API.
pub fn oauth_token_file() -> Result<PathBuf, CommonError> {
    Ok(data_dir()?.join("oauth-token.json"))
}
