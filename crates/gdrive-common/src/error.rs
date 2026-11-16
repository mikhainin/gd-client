//! Error types shared across crates.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CommonError {
    #[error("could not determine the current user's home directory")]
    NoHomeDirectory,

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("cannot resolve local sync root {path}: {source}")]
    LocalRoot {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("local sync roots overlap: {first} and {second}; disable or remove one binding")]
    OverlappingRoots {
        first: std::path::PathBuf,
        second: std::path::PathBuf,
    },

    #[error("failed to parse configuration: {0}")]
    TomlDe(#[from] toml::de::Error),

    #[error("failed to serialize configuration: {0}")]
    TomlSer(#[from] toml::ser::Error),
}
