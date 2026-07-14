use std::time::Instant;
//Auth
use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use core_auth::Token;
use core_protocol::{errors::CorelamoError, format::Format};
use uuid::Uuid;

use crate::{AppState, http_response::HttpError};

//INFO: everything later will need these two to return
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub format: Format,
    pub request_id: Uuid,
    pub instance: String,
    pub time_start: Instant,
}

//WARN: hardcodes json here, fix when json done
fn resolve_format(state: &AppState, request: &Request) -> Result<Format, String> {
    return Ok(Format::JSON);
    //TODO: start where xml detected
    todo!();
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
                Format::try_from(subtype).map_err(|_| subtype.to_string())
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
    let start = Instant::now();
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
                time_start: start,
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
        time_start: start,
    });

    next.run(request).await
}

//TODO: auth/https before request (check permissions....)
pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get("X-Corelamo-Key")
        .and_then(|v| v.to_str().ok());

    match token {
        Some(t) if state.auth.authenticate(&Token(t.to_string())).is_some() => {
            next.run(request).await
        }
        _ => (StatusCode::UNAUTHORIZED, "missing or invalid api key").into_response(),
    }
}
