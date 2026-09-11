//! Shared configuration model, filesystem paths and error types used by both
//! the `gdrived` background service and the `gdrive-ui` configuration app.

pub mod config;
pub mod dbus_api;
pub mod error;
pub mod paths;

pub use config::{AppConfig, SyncFolder};
pub use error::CommonError;
