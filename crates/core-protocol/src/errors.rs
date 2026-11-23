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

    #[error("database not running: {0}")]
    DatabaseNotRunning(String),

    #[error("database already running: {0}")]
    DatabaseAlreadyRunning(String),
    #[error("Busy:{0}")]
    Busy(String),
    #[error("failed: {0}")]
    FailedToEx(String),
}

//INFO: helpers to get all needed info from an error to http response
impl CorelamoError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "not_found",
            Self::AlreadyExists(_) => "already_exists",
            Self::InvalidData(_) => "invalid_data",
            Self::Internal(_) => "internal_error",
            Self::Unauthorized(_) => "unauthorized",
            Self::PermissionDenied(_) => "permission_denied",
            Self::UnsupportedFormat(_) => "unsupported_format",
            Self::Conflict(_) => "conflict",
            Self::UnknownRole(_) => "unknown_role",
            Self::DatabaseNotRunning(_) => "database_not_started",
            Self::PathNotIndexed(_) => "path_not_indexed",
            Self::DatabaseAlreadyRunning(_) => "database_already_started",
            Self::Busy(_) => "busy", //mosk janomaina
            Self::FailedToEx(_) => "execution_failed",
        }
    }

    #[must_use]
    pub const fn title(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "Not Found",
            Self::AlreadyExists(_) => "Already Exists",
            Self::InvalidData(_) => "Invalid Data",
            Self::Internal(_) => "Internal Error",
            Self::Unauthorized(_) => "Unauthorized",
            Self::PermissionDenied(_) => "Permission Denied",
            Self::UnsupportedFormat(_) => "Unsupported Format",
            Self::Conflict(_) => "Conflict",
            Self::UnknownRole(_) => "Role Error",
            Self::DatabaseNotRunning(_) => "Database Not Running",
            Self::PathNotIndexed(_) => "Path Not Indexed",
            Self::DatabaseAlreadyRunning(_) => "Database Already Running",
            Self::Busy(_) => "Service Unavailable",
            Self::FailedToEx(_) => "Execution failed",
        }
    }

    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::NotFound(msg)
            | Self::AlreadyExists(msg)
            | Self::InvalidData(msg)
            | Self::Internal(msg)
            | Self::Unauthorized(msg)
            | Self::PermissionDenied(msg)
            | Self::UnsupportedFormat(msg)
            | Self::Conflict(msg)
            | Self::DatabaseNotRunning(msg)
            | Self::DatabaseAlreadyRunning(msg)
            | Self::PathNotIndexed(msg)
            | Self::UnknownRole(msg)
            | Self::Busy(msg)
            | Self::FailedToEx(msg) => msg.clone(),
        }
    }
}

//more helpers to convert io/serde/toml
impl From<std::io::Error> for CorelamoError {
    fn from(e: std::io::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

impl From<simd_json::Error> for CorelamoError {
    fn from(e: simd_json::Error) -> Self {
        Self::InvalidData(e.to_string())
    }
}

impl From<serde_json::Error> for CorelamoError {
    fn from(e: serde_json::Error) -> Self {
        Self::InvalidData(e.to_string())
    }
}

impl From<toml::de::Error> for CorelamoError {
    fn from(e: toml::de::Error) -> Self {
        Self::InvalidData(e.message().to_owned())
    }
}

impl From<toml::ser::Error> for CorelamoError {
    fn from(e: toml::ser::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

//Meant for batch insert/retrieve/update so that we can like tell the user what happened to each document
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum FailReason {
    #[error("invalid json: {0}")]
    InvalidJson(String),

    #[error("missing id field '{field}' (auto_increment is off)")]
    MissingId { field: String },

    #[error("invalid value for field '{field}': expected {expected}, got '{got}'")]
    InvalidField {
        field: String,
        expected: String,
        got: String,
    },

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
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidJson(_) => "invalid_json",
            Self::MissingId { .. } => "missing_id",
            Self::NoIdField => "no_id_field",
            Self::DuplicatePrimaryId => "duplicate_primary_id",
            Self::NotFound => "not_found",
            Self::Internal { .. } => "internal_error",
            Self::InvalidField { .. } => "invalid_field",
        }
    }

    #[must_use]
    pub const fn status(&self) -> u16 {
        match self {
            Self::InvalidJson(_) => 400,
            Self::MissingId { .. } => 400,
            Self::InvalidField { .. } => 400,
            Self::NoIdField => 400,
            Self::DuplicatePrimaryId => 409,
            Self::NotFound => 404,
            Self::Internal { .. } => 500,
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
    #[must_use]
    pub const fn new(index: Option<usize>, id: Option<String>, reason: FailReason) -> Self {
        Self { index, id, reason }
    }

    #[must_use]
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
