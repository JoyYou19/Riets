use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CorelamoError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("already exists: {0}")]
    AlreadyExists(String),

    #[error("invalid data: {0}")]
    InvalidData(String),

    #[error("internal error: {0}")]
    Internal(String),

    //INFO: for auth - unauthorized
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    //INFO: for system level - permission_denied
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("unknown role:{0}")]
    UnknownRole(String),

    #[error("unsupported format: {0}")]
    UnsupportedFormat(String),

    #[error("conflict: {0}")]
    Conflict(String),
}

//INFO: helpers to get all needed info from an error to http response
impl CorelamoError {
    pub fn code(&self) -> &'static str {
        match self {
            CorelamoError::NotFound(_) => "not_found",
            CorelamoError::AlreadyExists(_) => "already_exists",
            CorelamoError::InvalidData(_) => "invalid_data",
            CorelamoError::Internal(_) => "internal_error",
            CorelamoError::Unauthorized(_) => "unauthorized",
            CorelamoError::PermissionDenied(_) => "permission_denied",
            CorelamoError::UnsupportedFormat(_) => "unsupported_format",
            CorelamoError::Conflict(_) => "conflict",
            CorelamoError::UnknownRole(_) => "unknown_role",
            
        }
    }

    pub fn title(&self) -> &'static str {
        match self {
            CorelamoError::NotFound(_) => "Not Found",
            CorelamoError::AlreadyExists(_) => "Already Exists",
            CorelamoError::InvalidData(_) => "Invalid Data",
            CorelamoError::Internal(_) => "Internal Error",
            CorelamoError::Unauthorized(_) => "Unauthorized",
            CorelamoError::PermissionDenied(_) => "Permission Denied",
            CorelamoError::UnsupportedFormat(_) => "Unsupported Format",
            CorelamoError::Conflict(_) => "Conflict",
            CorelamoError::UnknownRole(_) => "Role Error"
        }
    }

    pub fn message(&self) -> String {
        match self {
            CorelamoError::NotFound(msg)
            | CorelamoError::AlreadyExists(msg)
            | CorelamoError::InvalidData(msg)
            | CorelamoError::Internal(msg)
            | CorelamoError::Unauthorized(msg)
            | CorelamoError::PermissionDenied(msg)
            | CorelamoError::UnsupportedFormat(msg)
            | CorelamoError::Conflict(msg)
            | CorelamoError::UnknownRole(msg) =>msg.clone(),
        }
    }
}

//more helpers to convert io/serde/toml
impl From<std::io::Error> for CorelamoError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => CorelamoError::NotFound(e.to_string()),
            std::io::ErrorKind::AlreadyExists => CorelamoError::AlreadyExists(e.to_string()),
            std::io::ErrorKind::InvalidData | std::io::ErrorKind::InvalidInput => {
                CorelamoError::InvalidData(e.to_string())
            }
            // os-level permission failure is always a server-side config problem, not a user error
            std::io::ErrorKind::PermissionDenied => CorelamoError::PermissionDenied(e.to_string()),
            _ => CorelamoError::Internal(e.to_string()),
        }
    }
}

impl From<serde_json::Error> for CorelamoError {
    fn from(e: serde_json::Error) -> Self {
        match e.classify() {
            // Io category = serialization failure, our bug
            serde_json::error::Category::Io => CorelamoError::Internal(e.to_string()),
            // Syntax/Data/Eof = bad client input
            _ => CorelamoError::InvalidData(e.to_string()),
        }
    }
}

impl From<toml::de::Error> for CorelamoError {
    fn from(e: toml::de::Error) -> Self {
        CorelamoError::InvalidData(e.message().to_string())
    }
}

impl From<toml::ser::Error> for CorelamoError {
    fn from(e: toml::ser::Error) -> Self {
        CorelamoError::Internal(e.to_string())
    }
}

//TODO: From<xml_parser::Error> once we have XML

#[derive(Debug)]
pub struct CorelamoOk<T: Serialize> {
    pub title: String,
    pub data: Option<T>,
}

impl CorelamoOk<()> {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            data: None,
        }
    }
}

impl<T: Serialize> CorelamoOk<T> {
    pub fn with_data(title: impl Into<String>, data: T) -> Self {
        Self {
            title: title.into(),
            data: Some(data),
        }
    }
}
