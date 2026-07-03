//INFO: response.rs is responsible for sending messages in a constant format

use axum::{
    body::Body,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use core_protocol::{errors::CorelamoError, format::Format};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use crate::middleware::RequestContext;

//INFO: 🤓 RFC-7807 standart defines this Problem Detail for HTTP APIs:
// EXAMPLE:
// HTTP/1.1 403 Forbidden
//    Content-Type: application/problem+json
//    {
//     "type": "https://example.com/probs/out-of-credit",
//     "title": "You do not have enough credit.",
//     "detail": "Your current balance is 30, but that costs 50.",
//     "instance": "/account/12345/msgs/abc",
//     EXTRAS:
//     "balance": 30,
//     "accounts": ["/account/12345",
//                  "/account/67890"]
//    }
// INFO: we might remove some of these fields, currently just sticking to what "everyone uses"

//FIX: this is purely for the rfc standart so that a response can have the url we will definetly
//need to replace this later since this is a non-existent link
const DOCS_ROOT_URL: &str = "http://corelamo.com/errors/";

const REQUEST_ID_HEADER_NAME: &str = "x-corelamo-request-id";

fn error_to_status(err: &CorelamoError) -> StatusCode {
    match err {
        CorelamoError::NotFound(_) => StatusCode::NOT_FOUND,
        CorelamoError::AlreadyExists(_) => StatusCode::CONFLICT,
        CorelamoError::Conflict(_) => StatusCode::CONFLICT,
        CorelamoError::InvalidData(_) => StatusCode::BAD_REQUEST,
        CorelamoError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        CorelamoError::PermissionDenied(_) => StatusCode::INTERNAL_SERVER_ERROR,
        CorelamoError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
        CorelamoError::UnsupportedFormat(_) => StatusCode::NOT_ACCEPTABLE,
    }
}

pub struct HttpError {
    pub error_type: String,
    pub title: String,
    pub status: StatusCode,
    pub detail: String,
    pub instance: String,
    pub request_id: Uuid,
    pub format: Format,
}

impl HttpError {
    pub fn from_corelamo(err: CorelamoError, ctx: &RequestContext) -> Self {
        Self {
            error_type: format!("{}{}", DOCS_ROOT_URL, err.code()),
            title: err.title().to_string(),
            status: error_to_status(&err),
            detail: err.message(),
            instance: ctx.instance.clone(),
            request_id: ctx.request_id,
            format: ctx.format,
        }
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let body = json!({
            "type":       self.error_type,
            "title":      self.title,
            "status":     self.status.as_u16(),
            "detail":     self.detail,
            "instance":   self.instance,
            "request_id": self.request_id.to_string(),
        });

        let content_type = match self.format {
            Format::JSON => "application/problem+json",
            //FIX: serialize as XML once crate is picked, fall back to JSON for now
            Format::XML => "application/problem+xml",
        };

        Response::builder()
            .status(self.status)
            .header(header::CONTENT_TYPE, content_type)
            .header(REQUEST_ID_HEADER_NAME, self.request_id.to_string())
            .body(Body::from(
                //FIX:: same fix for xml needed here
                serde_json::to_vec_pretty(&body).unwrap_or_default(),
            ))
            .unwrap()
    }
}

pub struct HttpOk<T: Serialize> {
    pub status: StatusCode,
    pub title: String,
    pub data: Option<T>,
    pub request_id: Uuid,
    pub instance: String,
    pub format: Format,
}

impl HttpOk<()> {
    pub fn new(title: impl Into<String>, ctx: &RequestContext) -> Self {
        Self {
            status: StatusCode::OK,
            title: title.into(),
            data: None,
            request_id: ctx.request_id,
            instance: ctx.instance.clone(),
            format: ctx.format,
        }
    }

    pub fn with_status(status: StatusCode, title: impl Into<String>, ctx: &RequestContext) -> Self {
        Self {
            status,
            title: title.into(),
            data: None,
            request_id: ctx.request_id,
            instance: ctx.instance.clone(),
            format: ctx.format,
        }
    }

    //for data that doesnt need to be serialized like policy (TOML)
    pub fn raw(
        status: StatusCode,
        content_type: &'static str,
        body: String,
        ctx: &RequestContext,
    ) -> Response {
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, content_type)
            .header(REQUEST_ID_HEADER_NAME, ctx.request_id.to_string())
            .body(Body::from(body))
            .unwrap()
    }
}

//data constructors — T can be any Serialize type
impl<T: Serialize> HttpOk<T> {
    pub fn with_data(title: impl Into<String>, data: T, ctx: &RequestContext) -> Self {
        Self {
            status: StatusCode::OK,
            title: title.into(),
            data: Some(data),
            request_id: ctx.request_id,
            instance: ctx.instance.clone(),
            format: ctx.format,
        }
    }
}

impl<T: Serialize> IntoResponse for HttpOk<T> {
    fn into_response(self) -> Response {
        let mut body = json!({
            "status":     self.status.as_u16(),
            "title":      self.title,
            "instance":   self.instance,
            "request_id": self.request_id.to_string(),
        });

        if let Some(data) = &self.data {
            body["data"] = serde_json::to_value(data).unwrap_or(serde_json::Value::Null);
        }

        let content_type = match self.format {
            Format::JSON => "application/json",
            //TODO: serialize body as XML once crate is picked, fall back to JSON for now
            Format::XML => "application/json",
        };

        Response::builder()
            .status(self.status)
            .header(header::CONTENT_TYPE, content_type)
            .header(REQUEST_ID_HEADER_NAME, self.request_id.to_string())
            .body(Body::from(
                serde_json::to_vec_pretty(&body).unwrap_or_default(),
            ))
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    // ── From<io::Error> ──────────────────────────────────────────────────────

    #[test]
    fn test_io_not_found() {
        let e = io::Error::new(io::ErrorKind::NotFound, "missing");
        assert!(matches!(CorelamoError::from(e), CorelamoError::NotFound(_)));
    }

    #[test]
    fn test_io_already_exists() {
        let e = io::Error::new(io::ErrorKind::AlreadyExists, "exists");
        assert!(matches!(
            CorelamoError::from(e),
            CorelamoError::AlreadyExists(_)
        ));
    }

    #[test]
    fn test_io_invalid_data() {
        let e = io::Error::new(io::ErrorKind::InvalidData, "bad data");
        assert!(matches!(
            CorelamoError::from(e),
            CorelamoError::InvalidData(_)
        ));
    }

    #[test]
    fn test_io_invalid_input() {
        let e = io::Error::new(io::ErrorKind::InvalidInput, "bad input");
        assert!(matches!(
            CorelamoError::from(e),
            CorelamoError::InvalidData(_)
        ));
    }

    #[test]
    fn test_io_permission_denied_maps_to_permission_denied() {
        let e = io::Error::new(io::ErrorKind::PermissionDenied, "os denied");
        // OS permission failure is never a user auth issue — maps to PermissionDenied not Unauthorized
        assert!(matches!(
            CorelamoError::from(e),
            CorelamoError::PermissionDenied(_)
        ));
    }

    #[test]
    fn test_io_other_maps_to_internal() {
        let e = io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe");
        assert!(matches!(CorelamoError::from(e), CorelamoError::Internal(_)));
    }

    // ── From<serde_json::Error> ───────────────────────────────────────────────

    #[test]
    fn test_serde_json_deserialize_maps_to_invalid_data() {
        let e = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        assert!(matches!(
            CorelamoError::from(e),
            CorelamoError::InvalidData(_)
        ));
    }

    #[test]
    fn test_serde_json_syntax_maps_to_invalid_data() {
        let e = serde_json::from_str::<serde_json::Value>("{bad}").unwrap_err();
        assert!(matches!(
            CorelamoError::from(e),
            CorelamoError::InvalidData(_)
        ));
    }

    // ── From<toml::de::Error> ────────────────────────────────────────────────

    #[test]
    fn test_toml_de_error_maps_to_invalid_data() {
        let e = toml::from_str::<toml::Value>("not = [toml").unwrap_err();
        assert!(matches!(
            CorelamoError::from(e),
            CorelamoError::InvalidData(_)
        ));
    }

    // ── From<toml::ser::Error> ───────────────────────────────────────────────
    #[test]
    fn test_toml_ser_error_maps_to_internal() {
        // toml requires a table at the top level — a bare vec is invalid
        let e = toml::to_string(&vec![1, 2, 3]).unwrap_err();
        assert!(matches!(CorelamoError::from(e), CorelamoError::Internal(_)));
    }
    // ── code() / title() / message() ─────────────────────────────────────────

    #[test]
    fn test_code_all_variants() {
        assert_eq!(CorelamoError::NotFound("".into()).code(), "not_found");
        assert_eq!(
            CorelamoError::AlreadyExists("".into()).code(),
            "already_exists"
        );
        assert_eq!(CorelamoError::InvalidData("".into()).code(), "invalid_data");
        assert_eq!(CorelamoError::Internal("".into()).code(), "internal_error");
        assert_eq!(
            CorelamoError::Unauthorized("".into()).code(),
            "unauthorized"
        );
        assert_eq!(
            CorelamoError::PermissionDenied("".into()).code(),
            "permission_denied"
        );
        assert_eq!(
            CorelamoError::UnsupportedFormat("".into()).code(),
            "unsupported_format"
        );
        assert_eq!(CorelamoError::Conflict("".into()).code(), "conflict");
    }

    #[test]
    fn test_title_all_variants() {
        assert_eq!(CorelamoError::NotFound("".into()).title(), "Not Found");
        assert_eq!(
            CorelamoError::AlreadyExists("".into()).title(),
            "Already Exists"
        );
        assert_eq!(
            CorelamoError::InvalidData("".into()).title(),
            "Invalid Data"
        );
        assert_eq!(CorelamoError::Internal("".into()).title(), "Internal Error");
        assert_eq!(
            CorelamoError::Unauthorized("".into()).title(),
            "Unauthorized"
        );
        assert_eq!(
            CorelamoError::PermissionDenied("".into()).title(),
            "Permission Denied"
        );
        assert_eq!(
            CorelamoError::UnsupportedFormat("".into()).title(),
            "Unsupported Format"
        );
        assert_eq!(CorelamoError::Conflict("".into()).title(), "Conflict");
    }

    #[test]
    fn test_message_carries_inner_string() {
        assert_eq!(
            CorelamoError::NotFound("db not found".into()).message(),
            "db not found"
        );
        assert_eq!(
            CorelamoError::Internal("something broke".into()).message(),
            "something broke"
        );
    }
}
