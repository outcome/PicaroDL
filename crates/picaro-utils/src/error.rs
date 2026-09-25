//! Error types mirroring OrpheusDL's `utils/exceptions.py`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Module '{0}' authentication failed")]
    ModuleAuthError(String),

    #[error("Module {module} API error {code}: {message} (endpoint: {endpoint})")]
    ModuleApiError {
        module: String,
        code: i64,
        message: String,
        endpoint: String,
    },

    #[error("Module {module} general error: {message}")]
    ModuleGeneralError { module: String, message: String },

    #[error("Invalid input: {0}")]
    InvalidInput(String),

    #[error("Invalid module: {0}")]
    InvalidModule(String),

    #[error("Module '{module}' does not support ability '{ability}'")]
    ModuleDoesNotSupportAbility { module: String, ability: String },

    #[error("Module settings not set: {0}")]
    ModuleSettingsNotSet(String),

    #[error("Tag saving failure: {0}")]
    TagSavingFailure(String),

    #[error("Authentication error: {0}")]
    Authentication(String),

    #[error("Track unavailable: {0}")]
    TrackUnavailable(String),

    #[error("Artwork error: {0}")]
    Artwork(String),

    #[error("Download error: {0}")]
    Download(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("URL error: {0}")]
    Url(#[from] url::ParseError),

    #[error("TOML error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("Other error: {0}")]
    Other(String),

    #[error("{0}")]
    Generic(String),
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error::Other(s.to_string())
    }
}

impl Error {
    /// Convenience constructor for the dynamic `ModuleAPIError`.
    pub fn api(
        module: impl Into<String>,
        code: i64,
        message: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> Self {
        Error::ModuleApiError {
            module: module.into(),
            code,
            message: message.into(),
            endpoint: endpoint.into(),
        }
    }

    pub fn general(module: impl Into<String>, message: impl Into<String>) -> Self {
        Error::ModuleGeneralError {
            module: module.into(),
            message: message.into(),
        }
    }

    /// Many modules in the original raise arbitrary `Exception` objects. Wrap any
    /// string and turn it into a `ModuleGeneralError` for the current module.
    pub fn from_module_message(module: &str, msg: impl Into<String>) -> Self {
        Error::ModuleGeneralError {
            module: module.to_string(),
            message: msg.into(),
        }
    }
}

impl From<anyhow::Error> for Error {
    fn from(value: anyhow::Error) -> Self {
        Error::Other(value.to_string())
    }
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error::Other(s)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
