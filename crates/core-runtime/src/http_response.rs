//INFO: response.rs is responsible for sending messages in a constant format with the help of
use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use core_protocol::{
    command_reponse_definitions::ResponseData,
    errors::{CorelamoError, DocFailure},
    format::Format,
};

use simd_json::{OwnedValue, json, owned::Object};
use slog::{error, o, warn};
use std::time::Instant;

use uuid::Uuid;

use crate::middleware::RequestContext;

const DOCS_ROOT_URL: &str = "http://corelamo.com/errors/";
const REQUEST_ID_HEADER_NAME: &str = "x-corelamo-request-id";

struct SerializableData<T: serde::Serialize> {
    data: T,
}

impl<T: serde::Serialize> ResponseData for SerializableData<T> {
    fn to_json(&self) -> Result<OwnedValue, CorelamoError> {
        simd_json::serde::to_owned_value(&self.data)
            .map_err(|e| CorelamoError::Internal(format!("failed to serialize response data: {e}")))
    }
}

fn escape_json_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            //"pats" rakstiju:
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn error_to_status(err: &CorelamoError) -> StatusCode {
    match err {
        CorelamoError::NotFound(_) => StatusCode::NOT_FOUND,
        CorelamoError::AlreadyExists(_) => StatusCode::CONFLICT,
        CorelamoError::Conflict(_) => StatusCode::CONFLICT,
        CorelamoError::InvalidData(_) => StatusCode::BAD_REQUEST,
        CorelamoError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        CorelamoError::PermissionDenied(_) => StatusCode::FORBIDDEN,
        CorelamoError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
        CorelamoError::UnsupportedFormat(_) => StatusCode::NOT_ACCEPTABLE,
        CorelamoError::UnknownRole(_) => StatusCode::NOT_FOUND,
        CorelamoError::DatabaseAlreadyRunning(_) => StatusCode::CONFLICT,
        CorelamoError::PathNotIndexed(_) => StatusCode::CONFLICT,
        CorelamoError::DatabaseNotRunning(_) => StatusCode::CONFLICT,
        CorelamoError::Busy(_) => StatusCode::SERVICE_UNAVAILABLE,
        CorelamoError::FailedToEx(_) => StatusCode::INTERNAL_SERVER_ERROR,
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
    pub time_taken_ms: u64,
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
            time_taken_ms: ctx.time_start.elapsed().as_millis() as u64,
        }
    }

    fn from_render_failure(
        err: CorelamoError,
        instance: String,
        request_id: Uuid,
        format: Format,
        time_start: Instant,
    ) -> Self {
        Self {
            error_type: format!("{}{}", DOCS_ROOT_URL, err.code()),
            title: err.title().to_string(),
            status: error_to_status(&err),
            detail: err.message(),
            instance,
            request_id,
            format,
            time_taken_ms: time_start.elapsed().as_millis() as u64,
        }
    }
}

// HttpError for json and xml
impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let log = slog_scope::logger().new(o!("component"=>"middleware"));
        if self.status.is_server_error() {
            //console logging
            error!( log,
                "request failed";
                "status" => %self.status.as_u16(),
                "error_type" => %self.error_type,
                "detail" => %self.detail,
                "instance" => %self.instance,
               " request_id" => %self.request_id,

            );
        } else {
            warn!(log,
                "request rejected";
                "status" => self.status.as_u16(),
                "error_type" => %self.error_type,
                "detail "=> %self.detail,
                "instance" => %self.instance,
                "request_id" => %self.request_id,

            );
        }
        match self.format {
            Format::JSON => {
                let body = format!(
                    "{{\n  \"type\": \"{}\",\n  \"title\": \"{}\",\n  \"status\": {},\n  \"detail\": \"{}\",\n  \"instance\": \"{}\",\n  \"request_id\": \"{}\",\n  \"time_taken_ms\": {}\n}}",
                    escape_json_text(&self.error_type),
                    escape_json_text(&self.title),
                    self.status.as_u16(),
                    escape_json_text(&self.detail),
                    escape_json_text(&self.instance),
                    self.request_id,
                    self.time_taken_ms,
                );
                Response::builder()
                    .status(self.status)
                    .header(header::CONTENT_TYPE, "application/problem+json")
                    .header(REQUEST_ID_HEADER_NAME, self.request_id.to_string())
                    .body(Body::from(body))
                    .unwrap()
            } //Format::XML => todo!(),
        }
    }
}

//jo sis ir atrak :)
fn envelope_raw(
    status: StatusCode,
    title: &str,
    instance: &str,
    request_id: Uuid,
    time_start: Instant,
    fragment: &[u8],
) -> Vec<u8> {
    let time_taken = format!("{:?}", time_start.elapsed());
    let mut body = Vec::with_capacity(fragment.len() + 256);
    body.extend_from_slice(b"{\"status\":");
    body.extend_from_slice(status.as_u16().to_string().as_bytes());
    body.extend_from_slice(b",\"title\":\"");
    body.extend_from_slice(escape_json_text(title).as_bytes());
    body.extend_from_slice(b"\",\"instance\":\"");
    body.extend_from_slice(escape_json_text(instance).as_bytes());
    body.extend_from_slice(b"\",\"request_id\":\"");
    body.extend_from_slice(request_id.to_string().as_bytes());
    body.extend_from_slice(b"\",\"time_taken\":\"");
    body.extend_from_slice(escape_json_text(&time_taken).as_bytes());
    body.extend_from_slice(b"\",\"data\":");
    body.extend_from_slice(fragment);
    body.extend_from_slice(b"}");
    body
}

pub struct HttpOk {
    pub status: StatusCode,
    pub title: String,
    pub data: Option<Box<dyn ResponseData>>,
    pub request_id: Uuid,
    pub instance: String,
    pub format: Format,
    pub pretty: bool,
    pub time_start: Instant,
}

impl HttpOk {
    pub fn new(title: impl Into<String>, ctx: &RequestContext) -> Self {
        Self {
            status: StatusCode::OK,
            title: title.into(),
            data: None,
            request_id: ctx.request_id,
            pretty: ctx.pretty,
            instance: ctx.instance.clone(),
            format: ctx.format,
            time_start: ctx.time_start,
        }
    }

    pub fn with_status(status: StatusCode, title: impl Into<String>, ctx: &RequestContext) -> Self {
        Self {
            status,
            title: title.into(),
            data: None,
            request_id: ctx.request_id,
            pretty: ctx.pretty,
            instance: ctx.instance.clone(),
            format: ctx.format,
            time_start: ctx.time_start,
        }
    }

    pub fn with_data<T: serde::Serialize + 'static>(
        title: impl Into<String>,
        data: T,
        ctx: &RequestContext,
    ) -> Self {
        Self {
            status: StatusCode::OK,
            title: title.into(),
            data: Some(Box::new(SerializableData { data })),
            request_id: ctx.request_id,
            pretty: ctx.pretty,
            instance: ctx.instance.clone(),
            format: ctx.format,
            time_start: ctx.time_start,
        }
    }

    pub fn with_data_and_status<T: serde::Serialize + 'static>(
        status: StatusCode,
        title: impl Into<String>,
        data: T,
        ctx: &RequestContext,
    ) -> Self {
        Self {
            status,
            title: title.into(),
            data: Some(Box::new(SerializableData { data })),
            request_id: ctx.request_id,
            pretty: ctx.pretty,
            instance: ctx.instance.clone(),
            format: ctx.format,
            time_start: ctx.time_start,
        }
    }
    pub fn with_response(
        title: impl Into<String>,
        data: impl ResponseData + 'static,
        ctx: &RequestContext,
    ) -> Self {
        Self {
            status: StatusCode::OK,
            title: title.into(),
            data: Some(Box::new(data)),
            pretty: ctx.pretty,
            request_id: ctx.request_id,
            instance: ctx.instance.clone(),
            format: ctx.format,
            time_start: ctx.time_start,
        }
    }

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

impl IntoResponse for HttpOk {
    fn into_response(self) -> Response {
        match self.format {
            Format::JSON => {
                if !self.pretty {
                    if let Some(fragment) = self.data.as_ref().and_then(|d| d.to_raw_json()) {
                        let body = envelope_raw(
                            self.status,
                            &self.title,
                            &self.instance,
                            self.request_id,
                            self.time_start,
                            &fragment,
                        );
                        return Response::builder()
                            .status(self.status)
                            .header(header::CONTENT_TYPE, "application/json")
                            .header(REQUEST_ID_HEADER_NAME, self.request_id.to_string())
                            .body(Body::from(body))
                            .unwrap();
                    }
                }

                let data_value = match &self.data {
                    Some(d) => match d.to_json() {
                        Ok(v) => Some(v),
                        Err(e) => {
                            return HttpError::from_render_failure(
                                e,
                                self.instance,
                                self.request_id,
                                self.format,
                                self.time_start,
                            )
                            .into_response();
                        }
                    },
                    None => None,
                };

                let time_taken = format!("{:?}", self.time_start.elapsed());
                let mut obj = Object::new();
                obj.insert("status".into(), OwnedValue::from(self.status.as_u16()));
                obj.insert("title".into(), OwnedValue::String(self.title));
                obj.insert("instance".into(), OwnedValue::String(self.instance));
                obj.insert(
                    "request_id".into(),
                    OwnedValue::String(self.request_id.to_string()),
                );
                obj.insert("time_taken".into(), OwnedValue::String(time_taken.into()));
                if let Some(v) = data_value {
                    obj.insert("data".into(), v);
                }

                let body = if self.pretty {
                    simd_json::to_string_pretty(&OwnedValue::Object(obj.into()))
                } else {
                    simd_json::to_string(&OwnedValue::Object(obj.into()))
                }
                .unwrap_or_else(|_| "{}".to_string());

                Response::builder()
                    .status(self.status)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(REQUEST_ID_HEADER_NAME, self.request_id.to_string())
                    .body(Body::from(body))
                    .expect("Failed to build response")
            }
        }
    }
}

pub struct BatchOutcome {
    success_label: &'static str,
    all_failed_status: StatusCode,
    succeeded: u32,
    failures: Vec<DocFailure>,
}

impl BatchOutcome {
    pub fn new(success_label: &'static str, all_failed_status: StatusCode) -> Self {
        Self {
            success_label,
            all_failed_status,
            succeeded: 0,
            failures: Vec::new(),
        }
    }

    // pub fn succeed(&mut self) {
    //     self.succeeded += 1;
    // }

    pub fn succeed_many(&mut self, n: u32) {
        self.succeeded += n;
    }

    pub fn fail_doc(&mut self, failure: DocFailure) {
        self.failures.push(failure);
    }

    pub fn fail_many(&mut self, failures: impl IntoIterator<Item = DocFailure>) {
        self.failures.extend(failures);
    }

    pub fn succeeded_count(&self) -> u32 {
        self.succeeded
    }

    pub fn failed_count(&self) -> usize {
        self.failures.len()
    }

    pub fn has_failures(&self) -> bool {
        !self.failures.is_empty()
    }

    fn to_value(&self, db_name: &str) -> OwnedValue {
        let mut obj = Object::new();
        obj.insert(
            self.success_label.to_string(),
            OwnedValue::from(self.succeeded),
        );
        obj.insert(
            "database".to_string(),
            OwnedValue::String(db_name.to_string()),
        );

        if !self.failures.is_empty() {
            obj.insert(
                "failed".to_string(),
                OwnedValue::from(self.failures.len() as u64),
            );
            let results: Vec<OwnedValue> = self
                .failures
                .iter()
                .map(|f| {
                    json!({
                        "index": f.index,
                        "id": f.id,
                        "code": f.reason.code(),
                        "status": f.reason.status(),
                        "message": f.reason.to_string(),
                    })
                })
                .collect();
            obj.insert("results".to_string(), OwnedValue::Array(results.into()));
        }

        OwnedValue::Object(obj.into())
    }

    pub fn into_ok(
        mut self,
        clean_status: StatusCode,
        title: impl Into<String>,
        db_name: &str,
        ctx: &RequestContext,
    ) -> HttpOk {
        self.failures.sort_by_key(|f| f.index);

        let status = if !self.has_failures() {
            clean_status
        } else if self.succeeded == 0 {
            self.all_failed_status
        } else {
            StatusCode::MULTI_STATUS
        };
        let body = self.to_value(db_name);
        HttpOk::with_data_and_status(status, title, body, ctx)
    }
}
