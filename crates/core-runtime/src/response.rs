use axum::{Json, http::StatusCode};
use serde_json::{Value, json};

pub type ApiResponse = (StatusCode, Json<Value>);

//TODO: make responses pretier/constant easier to use

pub fn ok(message: &str) -> ApiResponse {
    (
        StatusCode::OK,
        Json(json!({ "status": "ok", "message": message })),
    )
}

pub fn created(message: &str) -> ApiResponse {
    (
        StatusCode::CREATED,
        Json(json!({ "status": "ok", "message": message })),
    )
}

pub fn ok_with_data(message: &str, data: Value) -> ApiResponse {
    (
        StatusCode::OK,
        Json(json!({ "status": "ok", "message": message, "data": data })),
    )
}

pub fn error(status: StatusCode, message: &str) -> ApiResponse {
    (
        status,
        Json(json!({ "status": "error", "message": message })),
    )
}

pub fn not_found(message: &str) -> ApiResponse {
    error(StatusCode::NOT_FOUND, message)
}

pub fn bad_request(message: &str) -> ApiResponse {
    error(StatusCode::BAD_REQUEST, message)
}

pub fn internal_error(message: &str) -> ApiResponse {
    error(StatusCode::INTERNAL_SERVER_ERROR, message)
}

pub fn conflict(message: &str) -> ApiResponse {
    error(StatusCode::CONFLICT, message)
}
