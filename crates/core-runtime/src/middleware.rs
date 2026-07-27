use std::time::Instant;
//Auth
use crate::{http_response::HttpError, AppState};
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use core_auth::{Principal, Token};
use core_protocol::{
    errors::CorelamoError::{self},
    format::Format,
};
use slog::{o, warn};
use uuid::Uuid;

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
    let log = slog_scope::logger().new(o!("component"=> "middleware"));
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
            return {
                warn!(log,"The format is unsupported";"format"=>%subtype);
                HttpError::from_corelamo(
                    CorelamoError::UnsupportedFormat(format!("unsupported format: '{subtype}'")),
                    &ctx,
                )
                .into_response()
            };
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
    mut request: Request,
    next: Next,
) -> Response {
    let log = slog_scope::logger().new(o!("component"=> "middleware"));
    let token: Option<String> = request
        .headers()
        .get("X-Corelamo-Key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let Some(token) = token else {
        warn!(log,"rejected request: missing auth token";
        "method" => %request.method(),
        "uri" => %request.uri(),);
        return (StatusCode::UNAUTHORIZED, "missing token").into_response();
    };

    let principal = {
        let Ok(auth) = state.auth.read() else {
            warn!( log,"rejected request: auth service unavailable";
              "method"=>%request.method(),
              "uri"=>%request.uri());
            return (StatusCode::UNAUTHORIZED, "auth service unavailable").into_response();
        };
        auth.authenticate(&Token(token))
    }; // <- auth (the lock guard) is dropped here, before any .await

    match principal {
        Some(principal) => {
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        None => {
            warn!(log,"rejected request: invalid or exipired token";"request"=>?request);
            (StatusCode::UNAUTHORIZED, "invalid or expired token").into_response()
        }
    }
}

pub async fn disabled_auth_middleware(mut request: Request, next: Next) -> Response {
    let principal = Principal::new("anonymous").with_role("admin");

    request.extensions_mut().insert(principal);

    next.run(request).await
}
