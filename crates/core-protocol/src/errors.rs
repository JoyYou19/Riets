use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, Serialize, Deserialize)]
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

    #[error("path not indexed: {0}")]
    PathNotIndexed(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("conflict: {0}")]
    DatabaseNotRunning(String),

    #[error("conflict: {0}")]
    DatabaseAlreadyRunning(String),
    #[error("Busy:{0}")]
    Busy(String),
    #[error("failed: {0}")]
    FailedToEx(String),

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
            CorelamoError::DatabaseNotRunning(_) => "database_not_started",
            CorelamoError::PathNotIndexed(_) => "path_not_indexed",
            CorelamoError::DatabaseAlreadyRunning(_) => "database_already_started",
            CorelamoError::Busy(_) => "Process is already happening", //mosk janomaina
            CorelamoError::FailedToEx(_) => "Failed to execute the task",
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
            CorelamoError::UnknownRole(_) => "Role Error",
            CorelamoError::DatabaseNotRunning(_) => "database_not_started",
            CorelamoError::PathNotIndexed(_) => "Path Not Indexed",
            CorelamoError::DatabaseAlreadyRunning(_) => "database_already_started",
            CorelamoError::Busy(_) => "Service Unavailable",
            CorelamoError::FailedToEx(_) => "Execution failed"
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
            | CorelamoError::DatabaseNotRunning(msg)
            | CorelamoError::DatabaseAlreadyRunning(msg)
            | CorelamoError::PathNotIndexed(msg)
            | CorelamoError::UnknownRole(msg)
            |CorelamoError::Busy(msg)
            |CorelamoError::FailedToEx(msg)=> msg.clone(),
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
            _ => CorelamoError::PermissionDenied(e.to_string()),
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

//Meant for batch insert/retrieve/update so that we can like tell the user what happened to each document
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum FailReason {
    #[error("invalid json: {0}")]
    InvalidJson(String),

    #[error("missing id field '{field}' (auto_increment is off)")]
    MissingId { field: String },

    #[error("policy has no id field declared")]
    NoIdField,

    #[error("duplicate primary id")]
    DuplicatePrimaryId,

    #[error("not found")]
    NotFound,

    #[error("internal error: {0}")]
    Internal(String),
}

impl FailReason {
    pub fn code(&self) -> &'static str {
        match self {
            FailReason::InvalidJson(_) => "invalid_json",
            FailReason::MissingId { .. } => "missing_id",
            FailReason::NoIdField => "no_id_field",
            FailReason::DuplicatePrimaryId => "duplicate_primary_id",
            FailReason::NotFound => "not_found",
            FailReason::Internal { .. } => "internal_error",
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            FailReason::InvalidJson(_) => 400,
            FailReason::MissingId { .. } => 400,
            FailReason::NoIdField => 400,
            FailReason::DuplicatePrimaryId => 409,
            FailReason::NotFound => 404,
            FailReason::Internal { .. } => 500,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocFailure {
    pub index: Option<usize>,
    pub id: Option<String>,
    pub reason: FailReason,
}

impl DocFailure {
    pub fn new(index: Option<usize>, id: Option<String>, reason: FailReason) -> Self {
        Self { index, id, reason }
    }

    pub fn at(index: usize, reason: FailReason) -> Self {
        Self {
            index: index.into(),
            id: None,
            reason,
        }
    }

    pub fn with_id(index: usize, id: impl Into<String>, reason: FailReason) -> Self {
        Self {
            index: Some(index),
            id: Some(id.into()),
            reason,
        }
    }
}
