use serde::{Deserialize, Serialize};

use crate::errors::CorelamoError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Format {
    JSON = 1,
    //XML = 2,
}

impl TryFrom<&str> for Format {
    type Error = CorelamoError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.to_ascii_lowercase().as_str() {
            "json" => Ok(Format::JSON),
            //"xml" => Ok(Format::XML),
            other => Err(CorelamoError::UnsupportedFormat(format!(
                "unsupported format: '{other}'"
            ))),
        }
    }
}

impl From<Format> for u8 {
    fn from(format: Format) -> Self {
        format as u8
    }
}

impl TryFrom<u8> for Format {
    type Error = CorelamoError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Format::JSON),
            //2 => Ok(Format::XML),
            other => Err(CorelamoError::InvalidData(format!(
                "unknown document format id {other}"
            ))),
        }
    }
}

impl From<CorelamoError> for std::io::Error {
    fn from(err: CorelamoError) -> Self {
        use std::io::{Error, ErrorKind};

        match err {
            CorelamoError::NotFound(msg) => Error::new(ErrorKind::NotFound, msg),
            CorelamoError::AlreadyExists(msg) => Error::new(ErrorKind::AlreadyExists, msg),
            CorelamoError::InvalidData(msg) => Error::new(ErrorKind::InvalidData, msg),
            CorelamoError::PermissionDenied(msg) => Error::new(ErrorKind::PermissionDenied, msg),
            CorelamoError::UnsupportedFormat(msg) => Error::new(ErrorKind::InvalidData, msg),
            CorelamoError::DatabaseAlreadyRunning(msg) => Error::other( msg),
            CorelamoError::DatabaseNotRunning(msg) => Error::other( msg),
            CorelamoError::Conflict(msg)
            | CorelamoError::UnknownRole(msg)
            | CorelamoError::Unauthorized(msg)
            | CorelamoError::Internal(msg) => Error::other(msg),
        }
    }
}
