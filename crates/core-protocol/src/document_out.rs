use simd_json::OwnedValue;
use simd_json::owned::Object;
use std::sync::Arc;

use crate::errors::CorelamoError;

#[derive(Debug, Clone)]
pub enum DocumentOut {
    Raw(Arc<[u8]>),
    Json(OwnedValue),
}

impl DocumentOut {
    pub fn raw_bytes(&self) -> Option<&[u8]> {
        match self {
            DocumentOut::Raw(bytes) => Some(bytes.as_ref()),
            DocumentOut::Json(_) => None,
        }
    }

    pub fn into_value(self, strip_id: Option<&str>) -> Result<OwnedValue, CorelamoError> {
        let mut value = match self {
            DocumentOut::Raw(bytes) => parse_source(&bytes)?,
            DocumentOut::Json(value) => value,
        };
        if let Some(path) = strip_id {
            strip_field_path(&mut value, path);
        }
        Ok(value)
    }

    pub fn to_value(&self, strip_id: Option<&str>) -> Result<OwnedValue, CorelamoError> {
        let mut value = match self {
            DocumentOut::Raw(bytes) => parse_source(bytes)?,
            DocumentOut::Json(value) => value.clone(),
        };
        if let Some(path) = strip_id {
            strip_field_path(&mut value, path);
        }
        Ok(value)
    }
}

fn parse_source(bytes: &[u8]) -> Result<OwnedValue, CorelamoError> {
    if bytes.is_empty() {
        return Ok(OwnedValue::Object(Box::new(Object::new())));
    }
    let mut buf = bytes.to_vec();
    simd_json::to_owned_value(&mut buf)
        .map_err(|e| CorelamoError::Internal(format!("stored document is not valid JSON: {e}")))
}

pub fn strip_field_path(value: &mut OwnedValue, path: &str) {
    match path.split_once('/') {
        None => {
            if let OwnedValue::Object(obj) = value {
                obj.remove(path);
            }
        }
        Some((head, rest)) => match value {
            OwnedValue::Object(obj) => {
                if let Some(child) = obj.get_mut(head) {
                    strip_field_path(child, rest);
                }
            }
            OwnedValue::Array(items) => {
                for child in items.iter_mut() {
                    strip_field_path(child, path);
                }
            }
            _ => {}
        },
    }
}
