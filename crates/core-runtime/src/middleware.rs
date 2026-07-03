use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use uuid::Uuid;

use crate::{AppState, doctypes, response::HttpError};
use core_core::errors::CorelamoError;

//INFO: everything later will need these two to return
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub format: doctypes::Format,
    pub request_id: Uuid,
    pub instance: String,
}

fn resolve_format(state: &AppState, request: &Request) -> Result<doctypes::Format, String> {
    let accept = request
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok());

    match accept {
        None => Ok(state.default_format),
        Some(accept) => {
            let first = accept.split(',').next().unwrap_or("").trim();
            let subtype = first
                .split('/')
                .nth(1)
                .unwrap_or("")
                .split(';')
                .next()
                .unwrap_or("")
                .trim();
            if first.is_empty() || subtype.is_empty() || subtype == "*" {
                Ok(state.default_format)
            } else {
                //TODO this try_from should also have everything xml related
                doctypes::Format::try_from(subtype).map_err(|_| subtype.to_string())
            }
        }
    }
}

//adds request_id and makes the RequestContext for other parts of programm
pub async fn request_context_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = Uuid::new_v4();
    let instance = request.uri().path().to_string();

    let format = match resolve_format(&state, &request) {
        Ok(f) => f,
        Err(subtype) => {
            // format resolution failed — respond using config default so we can
            // still produce a correctly formatted error response
            let ctx = RequestContext {
                format: state.default_format,
                request_id,
                instance,
            };
            return HttpError::from_corelamo(
                CorelamoError::UnsupportedFormat(format!("unsupported format: '{subtype}'")),
                &ctx,
            )
            .into_response();
        }
    };
    request.extensions_mut().insert(RequestContext {
        format,
        request_id,
        instance,
    });

    next.run(request).await
}

//TODO: auth/https before request (check permissions....)
pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let _token = request.headers().get("X-Corelamo-Key");

    //HACK: japieliek ka parbauda sis vienkarsi taads placeholder
    next.run(request).await

    // match token {
    //     Some(key) if key == "mysecretkey" => next.run(request).await,
    //     _ => (StatusCode::UNAUTHORIZED, "missing or invalid api key").into_response(),
    // }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppState;
    use crate::doctypes::Format;
    use axum::http::{Request, header};
    use std::{
        collections::HashMap,
        path::PathBuf,
        sync::{Arc, RwLock},
    };

    fn test_state(default_format: Format) -> AppState {
        AppState {
            // empty map — resolve_format only accesses default_format
            databases: Arc::new(RwLock::new(HashMap::new())),
            databases_dir: PathBuf::from("/tmp"),
            default_format,
        }
    }

    fn request_with_accept(accept: &str) -> Request<axum::body::Body> {
        Request::builder()
            .header(header::ACCEPT, accept)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    fn request_no_accept() -> Request<axum::body::Body> {
        Request::builder().body(axum::body::Body::empty()).unwrap()
    }

    // ── resolve_format ────────────────────────────────────────────────────────

    #[test]
    fn test_no_accept_header_uses_default() {
        let state = test_state(Format::JSON);
        let req = request_no_accept();
        assert_eq!(resolve_format(&state, &req).unwrap(), Format::JSON);
    }

    #[test]
    fn test_accept_application_json() {
        let state = test_state(Format::JSON);
        let req = request_with_accept("application/json");
        assert_eq!(resolve_format(&state, &req).unwrap(), Format::JSON);
    }

    #[test]
    fn test_accept_application_xml() {
        let state = test_state(Format::JSON);
        let req = request_with_accept("application/xml");
        assert_eq!(resolve_format(&state, &req).unwrap(), Format::XML);
    }

    #[test]
    fn test_accept_wildcard_uses_default() {
        let state = test_state(Format::JSON);
        let req = request_with_accept("*/*");
        assert_eq!(resolve_format(&state, &req).unwrap(), Format::JSON);
    }

    #[test]
    fn test_accept_unsupported_returns_err() {
        let state = test_state(Format::JSON);
        let req = request_with_accept("application/pdf");
        let e = resolve_format(&state, &req).unwrap_err();
        assert_eq!(e, "pdf");
    }

    #[test]
    fn test_accept_takes_first_of_multiple() {
        // we take the first listed type, ignoring q= weights — documented limitation
        let state = test_state(Format::JSON);
        let req = request_with_accept("application/xml, application/json;q=0.9");
        assert_eq!(resolve_format(&state, &req).unwrap(), Format::XML);
    }

    #[test]
    fn test_accept_with_charset_param_stripped() {
        // application/json;charset=utf-8 — charset must not confuse subtype parsing
        let state = test_state(Format::JSON);
        let req = request_with_accept("application/json;charset=utf-8");
        assert_eq!(resolve_format(&state, &req).unwrap(), Format::JSON);
    }

    #[test]
    fn test_default_xml_fallback() {
        // if config default is XML and no Accept header, use XML
        let state = test_state(Format::XML);
        let req = request_no_accept();
        assert_eq!(resolve_format(&state, &req).unwrap(), Format::XML);
    }
}
