use crate::errors::CorelamoError;
use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
#[repr(u8)]
pub enum Format {
    JSON = 1,
    //XML = 2,
}

impl TryFrom<&str> for Format {
    type Error = CorelamoError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.to_ascii_lowercase().as_str() {
            "json" => Ok(Self::JSON),
            //"xml" => Ok(Format::XML),
            other => Err(CorelamoError::UnsupportedFormat(format!(
                "unsupported format: '{other}'"
            ))),
        }
    }
}

impl From<Format> for u8 {
    fn from(format: Format) -> Self {
        format as Self
    }
}

impl TryFrom<u8> for Format {
    type Error = CorelamoError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::JSON),
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
            CorelamoError::NotFound(msg) => Self::new(ErrorKind::NotFound, msg),
            CorelamoError::AlreadyExists(msg) => Self::new(ErrorKind::AlreadyExists, msg),
            CorelamoError::InvalidData(msg) => Self::new(ErrorKind::InvalidData, msg),
            CorelamoError::PermissionDenied(msg) => Self::new(ErrorKind::PermissionDenied, msg),
            CorelamoError::UnsupportedFormat(msg) => Self::new(ErrorKind::InvalidData, msg),
            CorelamoError::DatabaseAlreadyRunning(msg) => Self::other(msg),
            CorelamoError::PathNotIndexed(msg) => Self::other(msg),
            CorelamoError::DatabaseNotRunning(msg) => Self::other(msg),
            CorelamoError::Conflict(msg)
            | CorelamoError::UnknownRole(msg)
            | CorelamoError::Unauthorized(msg)
            | CorelamoError::Internal(msg)
            | CorelamoError::Busy(msg)
            | CorelamoError::FailedToEx(msg) => Self::other(msg),
        }
    }
}
