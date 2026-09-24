use std::time::Instant;
//Auth
use crate::{AppState, http_response::HttpError};
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use core_auth::{Principal, Token};
//use core_auth::{Principal, Token};
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
    pub pretty: bool,
    pub request_id: Uuid,
    pub instance: String,
    pub time_start: Instant,
}

fn resolve_accept(state: &AppState, request: &Request) -> Result<(Format, Option<bool>), String> {
    let accept = request
        .headers()
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok());

    let mut pretty: Option<bool> = None;
    if let Some(accept) = accept {
        for media_range in accept.split(',') {
            let mut parts = media_range.split(';');
            let media = parts.next().unwrap_or("").trim();
            if !media.eq_ignore_ascii_case("application/json")
                && media != "*/*"
                && media != "application/*"
            {
                continue;
            }
            for param in parts {
                let param = param.trim();
                let Some((key, value)) = param.split_once('=') else {
                    continue;
                };
                if key.trim().eq_ignore_ascii_case("pretty") {
                    let value = value.trim();
                    if value.eq_ignore_ascii_case("false") {
                        pretty = Some(false);
                    } else if value.eq_ignore_ascii_case("true") {
                        pretty = Some(true);
                    }
                }
            }
        }
    }

    //HARDCODES JSON
    Ok((state.default_format, pretty))
}

pub async fn request_context_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = Uuid::new_v4();
    let start = Instant::now();
    let instance = request.uri().path().to_string();
    let log = slog_scope::logger().new(o!("component"=> "middleware"));
    let (format, pretty_override) = match resolve_accept(&state, &request) {
        Ok(v) => v,
        Err(subtype) => {
            let ctx = RequestContext {
                format: state.default_format,
                pretty: state.default_pretty,
                request_id,
                instance,
                time_start: start,
            };
            return {
                warn!(log, "The format is unsupported"; "format" => %subtype);
                HttpError::from_corelamo(
                    CorelamoError::UnsupportedFormat(format!("unsupported format: '{subtype}'")),
                    &ctx,
                )
                .into_response()
            };
        }
    };
    let pretty = pretty_override.unwrap_or(state.default_pretty);
    request.extensions_mut().insert(RequestContext {
        format,
        pretty,
        request_id,
        instance,
        time_start: start,
    });

    next.run(request).await
}

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
