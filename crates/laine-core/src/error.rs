use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LaineError {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Failed to parse configuration: {0}")]
    ConfigParse(#[from] toml::de::Error),

    #[error("Invalid configuration: {0}")]
    ConfigInvalid(String),

    #[error("Git command failed ({command}): {message}")]
    Git { command: String, message: String },

    #[error("Failed to parse OpenAPI specification: {0}")]
    OpenApiParse(String),

    #[error("Cache error: {0}")]
    Cache(String),
}

pub type Result<T> = std::result::Result<T, LaineError>;
