use crate::{
    AppState,
    database_helpers::{compute_disk_usage, normalize_db_name, trash_dir_for, validate_db_name},
    database_manager::SlotState,
    doc_parser,
    http_response::{BatchOutcome, HttpError, HttpOk},
    middleware::RequestContext,
};
use axum::{
    Extension,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use core_auth::{Permission, Principal};
use core_backup::backup::BackupType;
use core_core::{DatabaseOptions, shard_manager::ShardManager};
use core_index::lsm::index_worker::ReindexStatus;
use core_protocol::{
    command_reponse_definitions::{
        Command, CreateDatabaseRequest, DeleteCommand, DidYouMeanRequest, GetLogsRequest,
        InfoWordsRequest, LoginResponse, LookupCommand, PartialReplaceCommand,
        RenameDatabaseRequest, RetrieveCommand, RetrieveResponse, SearchCommand, SearchResponse,
        TimingsRequest,
    },
    document_out::DocumentOut,
    errors::CorelamoError,
};
use core_query::dictionary::DictionaryDocument;
use core_storage::document_projections::id_path_to_strip;
use core_timing::timed;
use simd_json::owned::Object;
use simd_json::{OwnedValue, json};
use slog::{error, info, o};
use std::{collections::HashMap, sync::Arc};

//authorizations
use serde::Deserialize;
#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}
#[derive(Deserialize)]
struct CreateUserRequest {
    username: String,
    password: String,
    roles: Vec<String>,
}
#[derive(Deserialize)]
struct UpdatePasswordRequest {
    password: String,
}
#[derive(Deserialize)]
struct UpdateRolesRequest {
    roles: Vec<String>,
}

fn require_body(body: &str) -> Result<&str, CorelamoError> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        Err(CorelamoError::InvalidData(
            "request body is empty".to_string(),
        ))
    } else {
        Ok(trimmed)
    }
}

fn check_permission(
    state: &AppState,
    principal: &Principal,
    permission: Permission,
) -> Result<(), CorelamoError> {
    let auth = state
        .auth
        .read()
        .map_err(|_| CorelamoError::Internal("auth service unavailable".into()))?;
    auth.check(principal, permission)
}

pub(crate) async fn login_handler(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    body: String,
) -> Response {
    let body = match require_body(&body) {
        Ok(b) => b,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };
    let mut buf = body.as_bytes().to_vec();
    let req: LoginRequest = match simd_json::from_slice(&mut buf) {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::InvalidData(format!("invalid login request:{e}")),
                &ctx,
            )
            .into_response();
        }
    };

    let token = {
        let Ok(mut auth) = state.auth.write() else {
            return HttpError::from_corelamo(
                CorelamoError::Internal("auth service lock poisoned".to_string()),
                &ctx,
            )
            .into_response();
        };
        auth.login(&req.username, &req.password)
    }; // write guard dropped here

    match token {
        Some(token) => {
            let resp = LoginResponse { token: token.0 };
            HttpOk::with_response("Login successful".to_string(), resp, &ctx).into_response()
        }
        None => HttpError::from_corelamo(
            CorelamoError::Unauthorized("invalid username or password".to_string()),
            &ctx,
        )
        .into_response(),
    }
}

pub(crate) async fn list_users_handler(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::ListUsers) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let auth = state.auth.read().unwrap_or_else(|e| e.into_inner());
    let users = auth.list_users();

    let data: Vec<_> = users
        .into_iter()
        .map(|(username, roles)| json!({ "username": username, "roles": roles }))
        .collect();

    HttpOk::with_data(
        format!("{} user(s)", data.len()),
        json!({ "users": data }),
        &ctx,
    )
    .into_response()
}

pub(crate) async fn create_user_handler(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::CreateUser) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }
    let body = match require_body(&body) {
        Ok(b) => b,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };
    let mut buf = body.as_bytes().to_vec();
    let req: CreateUserRequest = match simd_json::from_slice(&mut buf) {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::InvalidData(format!("invalid create-user request: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    let Ok(mut auth) = state.auth.write() else {
        return HttpError::from_corelamo(
            CorelamoError::Internal("auth service lock poisoned".to_string()),
            &ctx,
        )
        .into_response();
    };
    match auth.create_user(&principal, &req.username, &req.password, req.roles) {
        Ok(()) => HttpOk::new(format!("user '{}' created", req.username), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub(crate) async fn delete_user_handler(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::DeleteUser) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }
    let mut auth = state.auth.write().unwrap_or_else(|e| e.into_inner());
    match auth.delete_user(&principal, &username) {
        Ok(()) => HttpOk::new(format!("user '{}' deleted", username), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub(crate) async fn update_user_password_handler(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::UpdatePwd) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let body = match require_body(&body) {
        Ok(b) => b,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };
    let mut buf = body.as_bytes().to_vec();
    let req: UpdatePasswordRequest = match simd_json::from_slice(&mut buf) {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::InvalidData(format!("invalid update-password request: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    let mut auth = state.auth.write().unwrap_or_else(|e| e.into_inner());
    match auth.update_user_password(&principal, &username, &req.password) {
        Ok(()) => HttpOk::new(format!("password updated for '{}'", username), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub(crate) async fn update_user_roles_handler(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::UpdateRole) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let body = match require_body(&body) {
        Ok(b) => b,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };
    let mut buf = body.as_bytes().to_vec();
    let req: UpdateRolesRequest = match simd_json::from_slice(&mut buf) {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::InvalidData(format!("invalid update-roles request: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    let mut auth = state.auth.write().unwrap_or_else(|e| e.into_inner());
    match auth.update_user_roles(&principal, &username, req.roles) {
        Ok(()) => HttpOk::new(format!("roles updated for '{}'", username), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

// //TODO: total_hits: xxx kkadu
#[timed(search)]
pub(crate) async fn search_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::Search) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }
    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    if !handle.all_running() {
        return HttpError::from_corelamo(
            CorelamoError::DatabaseNotRunning(format!("database {db_name} is not running")),
            &ctx,
        )
        .into_response();
    }

    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let command: SearchCommand = match SearchCommand::parse(&body, ctx.format) {
        Ok(cmd) => cmd,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let query = command.query.clone();
    let start = std::time::Instant::now();
    let manager = Arc::clone(&handle);
    let hits = match manager.search(&command).await {
        Ok(hits) => hits,
        Err(e) => {
            handle.record_search(true, start.elapsed());
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    handle.record_search(false, start.elapsed());
    let hit_count = hits.0.len();
    let total_hits = hits.1;

    let policy = handle.policy();
    let strip_id = id_path_to_strip(
        policy.id_field().map(|f| f.name.as_str()),
        command.return_fields.as_ref(),
    )
    .map(String::from);

    let docs: Vec<(String, f32, DocumentOut, Vec<String>)> = hits
        .0
        .into_iter()
        .map(|hit| (hit.external_id, hit.score, hit.doc, hit.highlights))
        .collect();

    let resp = SearchResponse::new(docs, strip_id, total_hits);

    HttpOk::with_response(format!("{hit_count} hit(s) for '{query:?}'"), resp, &ctx).into_response()
}

#[timed(search)]
pub(crate) async fn did_you_mean_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::DidYouMean) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    if !handle.all_running() {
        return HttpError::from_corelamo(
            CorelamoError::DatabaseNotRunning(format!("database {db_name} is not running")),
            &ctx,
        )
        .into_response();
    }

    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let mut buf = body.as_bytes().to_vec();
    let req: DidYouMeanRequest = match simd_json::from_slice(&mut buf) {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::InvalidData(format!("invalid did-you-mean request: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    let report = match handle.did_you_mean(&req).await {
        Ok(r) => r,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let found: usize = report.words.iter().map(|w| w.suggestions.len()).sum();

    let suggestions: Vec<OwnedValue> = report
        .words
        .iter()
        .map(|w| {
            json!({
                "word": w.word,
                "suggestions": w.suggestions,
            })
        })
        .collect();

    HttpOk::with_data(
        format!("{found} suggestion(s) for '{}'", report.original),
        json!({
            "original": report.original,
            "corrected": report.corrected,
            "suggestions": suggestions,
        }),
        &ctx,
    )
    .into_response()
}

pub(crate) async fn get_all_fields_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::AllFields) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let all_fields = handle.all_fields();

    let mut fields_json = Object::new();
    for (xpath, kind) in all_fields.get_fields().iter() {
        fields_json.insert(xpath.clone().into(), simd_json::json!(kind.label()));
    }
    let fields_json = OwnedValue::Object(fields_json.into());

    HttpOk::with_data(
        format!("{} field(s) for '{db_name}'", all_fields.len()),
        json!({ "fields": fields_json }),
        &ctx,
    )
    .into_response()
}

#[timed(retrieve_opps)]
pub(crate) async fn lookup_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::Lookup) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    if !handle.all_running() {
        return HttpError::from_corelamo(
            CorelamoError::DatabaseNotRunning(format!("database {db_name} is not running")),
            &ctx,
        )
        .into_response();
    }

    let command = match LookupCommand::parse(&body, ctx.format) {
        Ok(cmd) => cmd,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let result = handle.lookup(&command).await;

    let resp = match result {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let hit_count = resp.docs.len();
    HttpOk::with_response(format!("looked up {hit_count} document(s)"), resp, &ctx).into_response()
}

#[timed(inserting)]
pub(crate) async fn insert_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::Insert) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }
    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    if !handle.all_running() {
        return HttpError::from_corelamo(
            CorelamoError::DatabaseNotRunning(format!("database {db_name} is not running")),
            &ctx,
        )
        .into_response();
    }

    let policy = handle.policy();

    let format = ctx.format;
    let parsed =
        tokio::task::spawn_blocking(move || doc_parser::parse_documents(&body, format, &policy))
            .await;

    let outcome = match parsed {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("parse task panicked: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    let doc_indices = outcome.indices;
    let parse_failures = outcome.failures;

    let report = match handle.insert(outcome.docs, principal.id.0.clone()).await {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };
    let mut outcome = BatchOutcome::new("inserted", StatusCode::CONFLICT);
    outcome.succeed_many(report.inserted);
    outcome.fail_many(parse_failures);

    // storage indexes into the parsed vec; map back to the client's input array
    for mut failure in report.failures {
        failure.index = failure.index.and_then(|i| doc_indices.get(i).copied());
        outcome.fail_doc(failure);
    }

    let title = format!(
        "inserted {} into '{db_name}', {} failed",
        outcome.succeeded_count(),
        outcome.failed_count()
    );
    outcome
        .into_ok(StatusCode::OK, title, &db_name, &ctx)
        .into_response()
}

#[timed(retrieve_opps)]
pub(crate) async fn retrieve_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::Retrieve) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let command = match RetrieveCommand::parse(&body, ctx.format) {
        Ok(cmd) => cmd,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    if !handle.all_running() {
        return HttpError::from_corelamo(
            CorelamoError::DatabaseNotRunning(format!("database {db_name} is not running")),
            &ctx,
        )
        .into_response();
    }

    let manager = Arc::clone(&handle);
    let ids = command.ids;
    let results = match manager.retrieve(ids).await {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let mut docs = Vec::new();
    let mut not_found_ids: Vec<String> = Vec::new();

    for (id, doc) in results {
        match doc {
            Some(d) => docs.push(d),
            None => not_found_ids.push(id),
        }
    }

    let (documents, skipped_ids) = doc_parser::convert_from_storage(&docs, ctx.format);

    let title = if skipped_ids.is_empty() {
        format!("retrieved {} document(s)", documents.len())
    } else {
        format!(
            "retrieved {} document(s); {} skipped due to format mismatch",
            documents.len(),
            skipped_ids.len()
        )
    };

    let resp = RetrieveResponse::new(documents, not_found_ids, skipped_ids);

    HttpOk::with_response(title, resp, &ctx).into_response()
}

#[timed(database_lifecycle)]
pub(crate) async fn start_database_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::StartDB) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, _gate) = match state.db_write(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    match handle.start().await {
        Ok(()) => HttpOk::new(format!("database '{db_name}' started"), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

#[timed(database_lifecycle)]
pub(crate) async fn stop_database_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::StopDB) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, _gate) = match state.db_write(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    match handle.stop().await {
        Ok(()) => HttpOk::new(format!("database '{db_name}' stopped"), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

#[timed(database_lifecycle)]
pub(crate) async fn restart_database_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::RestartDB) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, _gate) = match state.db_write(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    match handle.restart().await {
        Ok(()) => {
            HttpOk::new(format!("database '{db_name}' succesfuly restarted"), &ctx).into_response()
        }
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

#[timed(database_lifecycle)]
pub(crate) async fn clear_database_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::ClearDB) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, _gate) = match state.db_write(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    match handle.clear_all().await {
        Ok(_) => HttpOk::new(
            format!("database: {db_name}, is cleared of data and index"),
            &ctx,
        )
        .into_response(),

        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub(crate) async fn get_logs_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::GetLogs) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let date = if body.trim().is_empty() {
        None
    } else {
        //insane smukais porno lai dabutu date: "" aaraa
        let mut buf = body.trim().as_bytes().to_vec();
        match simd_json::from_slice::<GetLogsRequest>(&mut buf) {
            Ok(req) => req.date,
            Err(e) => {
                return HttpError::from_corelamo(
                    CorelamoError::InvalidData(format!("invalid get-logs request: {e}")),
                    &ctx,
                )
                .into_response();
            }
        }
    };

    match handle.get_logs(date) {
        Ok(msg) => HttpOk::raw(StatusCode::OK, "text/plain", msg, &ctx),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub(crate) async fn clear_logs_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::ClearLogs) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    match handle.clear_logs() {
        Ok(_) => HttpOk::new(format!("Deleted logs for {db_name}"), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

#[timed(modifying_documents)]
pub(crate) async fn delete_document_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::Delete) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }
    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let command = match DeleteCommand::parse(&body, ctx.format) {
        Ok(cmd) => cmd,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    if !handle.all_running() {
        return HttpError::from_corelamo(
            CorelamoError::DatabaseNotRunning(format!("database {db_name} is not running")),
            &ctx,
        )
        .into_response();
    }

    let manager = Arc::clone(&handle);
    let ids = command.ids;

    let report = match manager.delete(ids, principal.id.0.clone()).await {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("delete task panicked: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    let mut outcome = BatchOutcome::new("deleted", StatusCode::NOT_FOUND);
    outcome.succeed_many(report.deleted);
    outcome.fail_many(report.failures);

    let title = format!(
        "deleted {} document(s) from '{db_name}', {} not found",
        outcome.succeeded_count(),
        outcome.failed_count()
    );

    outcome
        .into_ok(StatusCode::OK, title, &db_name, &ctx)
        .into_response()
}

#[timed(modifying_documents)]
pub(crate) async fn partial_replace_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::Replace) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let command = match PartialReplaceCommand::parse(&body, ctx.format) {
        Ok(cmd) => cmd,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    if !handle.all_running() {
        return HttpError::from_corelamo(
            CorelamoError::DatabaseNotRunning(format!("database {db_name} is not running")),
            &ctx,
        )
        .into_response();
    }

    //convert this to Vec<(id, value)>
    let items: Vec<(String, OwnedValue)> = command
        .items
        .into_iter()
        .map(|item| (item.id, item.patch))
        .collect();

    //now we fetch the docs while still in async
    let mut fetched = HashMap::with_capacity(items.len());
    for (id, _) in &items {
        if fetched.contains_key(id) {
            continue;
        }
        //WARN: this happens sequentially, for small partial-replaces its ok, if something happens we
        //can upgrade this

        let result = handle.retrieve_one(id).await.map_err(|e| e.to_string());
        fetched.insert(id.clone(), result);
    }

    let policy = handle.policy();
    let parsed = tokio::task::spawn_blocking(move || {
        doc_parser::parse_partial_replace_to_inputs(&items, &policy, |id| match fetched.get(id) {
            Some(Ok(doc)) => Ok(doc.clone()),
            Some(Err(msg)) => Err(CorelamoError::Internal(msg.clone())),
            None => Ok(None),
        })
    })
    .await;

    let (inputs, parse_failures) = match parsed {
        Ok(result) => result,
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("parse task panicked: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    if inputs.is_empty() {
        let mut outcome = BatchOutcome::new("partially replaced", StatusCode::NOT_FOUND);
        outcome.fail_many(parse_failures);
        let title = format!(
            "partially replaced 0 document(s) in '{db_name}', {} failed",
            outcome.failed_count()
        );
        return outcome
            .into_ok(StatusCode::OK, title, &db_name, &ctx)
            .into_response();
    }

    let report = match handle.replace(inputs, principal.id.0.clone()).await {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("replace task panicked: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    let mut outcome = BatchOutcome::new("partially replaced", StatusCode::NOT_FOUND);
    outcome.succeed_many(report.replaced);
    outcome.fail_many(parse_failures);
    outcome.fail_many(report.failures);

    let title = format!(
        "partially replaced {} document(s) in '{db_name}', {} failed",
        outcome.succeeded_count(),
        outcome.failed_count()
    );
    outcome
        .into_ok(StatusCode::OK, title, &db_name, &ctx)
        .into_response()
}

#[timed(modifying_documents)]
pub(crate) async fn replace_document_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::Replace) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    if !handle.all_running() {
        return HttpError::from_corelamo(
            CorelamoError::DatabaseNotRunning(format!("database {db_name} is not running")),
            &ctx,
        )
        .into_response();
    }

    let policy = handle.policy();
    let format = ctx.format;
    let parsed =
        tokio::task::spawn_blocking(move || doc_parser::parse_documents(&body, format, &policy))
            .await;

    let outcome = match parsed {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("parse task panicked: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    let doc_indices = outcome.indices;
    let parse_failures = outcome.failures;

    let manager = Arc::clone(&handle);
    let report = match manager.replace(outcome.docs, principal.id.0.clone()).await {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("replace task panicked: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    let mut outcome = BatchOutcome::new("replaced", StatusCode::NOT_FOUND);
    outcome.succeed_many(report.replaced);
    outcome.fail_many(parse_failures);

    for mut failure in report.failures {
        failure.index = failure.index.and_then(|i| doc_indices.get(i).copied());
        outcome.fail_doc(failure);
    }

    let title = format!(
        "replaced {} in '{db_name}', {} failed",
        outcome.succeeded_count(),
        outcome.failed_count()
    );
    outcome
        .into_ok(StatusCode::OK, title, &db_name, &ctx)
        .into_response()
}

#[timed(modifying_documents)]
pub(crate) async fn upsert_document_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::Upsert) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    if !handle.all_running() {
        return HttpError::from_corelamo(
            CorelamoError::DatabaseNotRunning(format!("database {db_name} is not running")),
            &ctx,
        )
        .into_response();
    }

    let policy = handle.policy();
    let format = ctx.format;
    let parsed =
        tokio::task::spawn_blocking(move || doc_parser::parse_documents(&body, format, &policy))
            .await;

    let outcome = match parsed {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("parse task panicked: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    let doc_indices = outcome.indices;
    let parse_failures = outcome.failures;

    let manager = Arc::clone(&handle);
    let report = match manager.upsert(outcome.docs, principal.id.0.clone()).await {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("upsert task panicked: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    let mut outcome = BatchOutcome::new("upserted", StatusCode::CONFLICT);
    outcome.succeed_many(report.inserted);
    outcome.fail_many(parse_failures);

    for mut failure in report.failures {
        failure.index = failure.index.and_then(|i| doc_indices.get(i).copied());
        outcome.fail_doc(failure);
    }

    let title = format!(
        "upserted {} document(s) into '{db_name}', {} failed",
        outcome.succeeded_count(),
        outcome.failed_count()
    );
    outcome
        .into_ok(StatusCode::OK, title, &db_name, &ctx)
        .into_response()
}

#[timed(database_lifecycle)]
pub(crate) async fn create_database_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::CreateDatabase) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    // the name is a registry key and a directory name, so it is folded once, here
    let db_name = normalize_db_name(&db_name);
    if let Err(e) = validate_db_name(&db_name) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let shard_count = (if body.trim().is_empty() {
        None
    } else {
        let mut buf = body.trim().as_bytes().to_vec();
        match simd_json::from_slice::<CreateDatabaseRequest>(&mut buf) {
            Ok(req) => req.shard_count,
            Err(e) => {
                return HttpError::from_corelamo(
                    CorelamoError::InvalidData(format!("invalid create-database request: {e}")),
                    &ctx,
                )
                .into_response();
            }
        }
    })
    //TODO: calculate in a smart way
    .unwrap_or(4);
    //studip max shard_count
    if shard_count > 128 {
        return HttpError::from_corelamo(
            CorelamoError::InvalidData(format!(
                "shard_count {shard_count} exceeds the maximum of 128"
            )),
            &ctx,
        )
        .into_response();
    }
    if shard_count < 1 {
        return HttpError::from_corelamo(
            CorelamoError::InvalidData("database must have atleast one shard".to_string()),
            &ctx,
        )
        .into_response();
    }

    //reserve name + already exists...
    let slot = match state.databases.reserve(&db_name) {
        Ok(slot) => slot,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };
    let db_path = slot.dir.clone();

    //build it on disk
    let created = tokio::task::spawn_blocking(move || {
        ShardManager::create(
            db_path,
            DatabaseOptions::default(),
            shard_count,
            crate::corelamo_settings::synonym_registry(),
            crate::corelamo_settings::model_registry(),
        )
    })
    .await;

    //smart publish + cleanup oon errors
    match created {
        Ok(Ok(manager)) => {
            let manager = Arc::new(manager);
            manager.start_backup_scheduler();
            slot.set_live(manager);
            HttpOk::with_status(
                StatusCode::CREATED,
                format!("database '{db_name}' created"),
                &ctx,
            )
            .into_response()
        }
        Ok(Err(e)) => {
            let _ = std::fs::remove_dir_all(&slot.dir);
            state.databases.remove(&db_name);
            HttpError::from_corelamo(e, &ctx).into_response()
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&slot.dir);
            state.databases.remove(&db_name);
            HttpError::from_corelamo(
                CorelamoError::Internal(format!("create task panicked: {e}")),
                &ctx,
            )
            .into_response()
        }
    }
}

#[timed(database_lifecycle)]
pub(crate) async fn delete_database_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::DeleteDatabase) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let log = slog_scope::logger().new(o!("components" => "handlers"));

    let slot = match state.databases.get(&db_name) {
        Ok(slot) => slot,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let _gate = slot.gate.write().await;

    let manager = match slot.manager() {
        Ok(manager) => manager,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    slot.set_state(SlotState::Deleting);

    let db = Arc::clone(&manager);
    match tokio::task::spawn_blocking(move || db.shutdown()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            slot.set_state(SlotState::Live);
            error!(log, "delete: shutdown failed, database kept"; "db" => %db_name, "error" => %e);
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!(
                    "failed to shut down '{db_name}' before deleting: {e}; \
                     it is stopped and was not removed"
                )),
                &ctx,
            )
            .into_response();
        }
        Err(e) => {
            slot.set_state(SlotState::Live);
            error!(log, "delete: shutdown task panicked"; "db" => %db_name, "error" => %e);
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("shutdown task panicked for '{db_name}': {e}")),
                &ctx,
            )
            .into_response();
        }
    }

    //we first move the "database" to deleting-database and then delete it for crash ssafety and
    //preventing a partially deleted database
    let dir = slot.dir.clone();
    let trash = trash_dir_for(state.databases.dir(), &db_name);
    let removed = tokio::task::spawn_blocking(move || -> Result<(), std::io::Error> {
        std::fs::rename(&dir, &trash)?;
        std::fs::remove_dir_all(&trash)
    })
    .await;

    match removed {
        Ok(Ok(())) => {
            state.databases.remove(&db_name);
            info!(log, "database deleted"; "name" => %db_name);
            HttpOk::new(format!("database '{db_name}' deleted"), &ctx).into_response()
        }
        Ok(Err(e)) if slot.dir.exists() => {
            // nothing moved, so the files are still ours: hand the database back stopped
            slot.set_state(SlotState::Live);
            error!(log, "database delete failed, database kept"; "name" => %db_name, "error" => %e);
            HttpError::from_corelamo(
                CorelamoError::Internal(format!(
                    "failed to delete '{db_name}' from disk: {e}; it is stopped and was not removed"
                )),
                &ctx,
            )
            .into_response()
        }
        Ok(Err(e)) => {
            // renamed into .deleting-, only the recursive removal failed
            state.databases.remove(&db_name);
            error!(log, "database delete incomplete"; "name" => %db_name, "error" => %e);
            HttpError::from_corelamo(
                CorelamoError::Internal(format!(
                    "removed '{db_name}' from memory but failed to delete its files: {e}"
                )),
                &ctx,
            )
            .into_response()
        }
        Err(e) => {
            // the task panicked, so whether the directory moved can only be read off the disk
            if slot.dir.exists() {
                slot.set_state(SlotState::Live);
            } else {
                state.databases.remove(&db_name);
            }
            error!(log, "database delete panicked"; "name" => %db_name, "error" => %e);
            HttpError::from_corelamo(
                CorelamoError::Internal(format!("delete task panicked: {e}")),
                &ctx,
            )
            .into_response()
        }
    }
}

pub(crate) async fn stats_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::Status) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    if !handle.all_running() {
        return HttpError::from_corelamo(
            CorelamoError::DatabaseNotRunning(format!("database {db_name} is not running")),
            &ctx,
        )
        .into_response();
    }

    // atomics only: this never queues behind an fsync or a reindex commit
    let stats = handle.stats();
    let indexing = &stats.indexing;
    let reindexing = &stats.reindexing;
    let metrics = &stats.metrics;
    let backup = &stats.backup;
    let background = &stats.background;
    HttpOk::with_data(
        format!("stats for '{db_name}'"),
        json!({
            "background_compaction_enabled": stats.background_compaction_enabled,
            "shard_count": stats.shard_count,
            "metrics": {
                "search_requests": metrics.search_requests,
                "search_errors": metrics.search_errors,
                "average_search_us": metrics.average_search_time()
                    .map(|d| d.as_millis() as u64),
                "indexing_requests": metrics.indexing_requests,
                "indexing_errors": metrics.indexing_errors,
                "average_indexing_ms": metrics.average_indexing_time()
                    .map(|d| d.as_millis() as u64),
                "reindex_requests": metrics.reindex_requests,
                "reindex_errors": metrics.reindex_errors,
            },
            "indexed": {
                "documents": stats.document_count,
                "deleted": indexing.total_documents_deleted,
                "segments": stats.segment_count,
            },
            "reindexing": {
                "status": reindexing.status,
                "progress": reindexing.progress,
                "documents_indexed": reindexing.documents_indexed,
                "eta_seconds": reindexing.eta_seconds,
            },
                "background": {
                "shards_compacting": background.shards_compacting,
                "shards_embedding": background.shards_embedding,
                "compactions_completed": indexing.compactions_completed,
                "compaction_failures": background.compaction_failures,
                "last_compaction_ms": background.last_compaction_ms,
                "embedding": {
                    "status": background.embedding.status.as_str(),
                    "progress": background.embedding.progress,
                    "vectors_total": background.embedding.vectors_total,
                    "vectors_done": background.embedding.vectors_done,
                    "docs_per_second": background.embedding.docs_per_second,
                    "eta_seconds": background.embedding.eta_seconds,
                },
            },
            "backup": {
            "status": backup.status,
            "progress_percent": backup.progress_percent,
            "mb_done": backup.mb_done,
            "mb_total": backup.mb_total,
            "eta_seconds": backup.eta_seconds,
            "restoring": stats.restoring,
            }
        }),
        &ctx,
    )
    .into_response()
}

#[timed(search)]
pub(crate) async fn info_words_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::Search) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }
    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    if !handle.all_running() {
        return HttpError::from_corelamo(
            CorelamoError::DatabaseNotRunning(format!("database {db_name} is not running")),
            &ctx,
        )
        .into_response();
    }

    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };
    let mut buf = body.as_bytes().to_vec();
    let req: InfoWordsRequest = match simd_json::from_slice(&mut buf) {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::InvalidData(format!("invalid info-words request: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    let stats = match handle.info_words(req.words).await {
        Ok(stats) => stats,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    // info_words_handler
    let results: Vec<OwnedValue> = stats
        .iter()
        .map(|s| {
            json!({
                "word": s.word,
                "occurrences": s.occurrences,
                "documents": s.documents,
            })
        })
        .collect();
    HttpOk::with_data(
        format!("info for {} word(s)", results.len()),
        json!({ "words": results }),
        &ctx,
    )
    .into_response()
}

#[timed(reindex)]
pub(crate) async fn reindex_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::Reindex) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, _gate) = match state.db_write(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    if !handle.all_running() {
        return HttpError::from_corelamo(
            CorelamoError::DatabaseNotRunning(format!("database {db_name} is not running")),
            &ctx,
        )
        .into_response();
    }

    let name_for_db = db_name.clone();
    let value = ctx.clone();
    let log = slog_scope::logger();

    let result = match tokio::task::spawn_blocking(move || handle.reindex()).await {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("reindex task panicked: {e}")),
                &value,
            )
            .into_response();
        }
    };

    match result {
        Ok(()) => {
            info!(log, "reindex prep finished, background reindexing started"; "db" => %name_for_db);
            HttpOk::new("reindex started, poll /status for progress", &value).into_response()
        }
        Err(e) => {
            error!(log, "reindex failed to start"; "db" => %name_for_db, "error" => %e);
            HttpError::from_corelamo(e, &value).into_response()
        }
    }
}
#[timed(reindex)]
pub(crate) async fn abort_reindex_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::Reindex) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, _gate) = match state.db_write(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    if handle.stats().reindexing.status == ReindexStatus::Cancelled {
        return HttpOk::new(format!("reindex already aborted for '{db_name}'"), &ctx)
            .into_response();
    }
    handle.abort_reindex();
    HttpOk::new(format!("reindex aborted for '{db_name}'"), &ctx).into_response()
}

pub(crate) async fn get_policy_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::GetPolicy) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };
    let policy = handle.policy();

    match doc_parser::serialize_policy(&policy) {
        Ok(output) => HttpOk::raw(StatusCode::OK, "application/json", output, &ctx),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub(crate) async fn set_policy_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::SetPolicy) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }
    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let policy = match doc_parser::parse_policy(&body) {
        Ok(p) => p,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let (handle, _gate) = match state.db_write(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    match handle.set_policy_all(policy, principal.id.0.clone()).await {
        Ok(()) => HttpOk::new(format!("policy updated for '{db_name}'"), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub(crate) async fn get_config_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::GetConfig) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let options = handle.options();

    match toml::to_string_pretty(&options) {
        Ok(output) => HttpOk::raw(StatusCode::OK, "application/toml", output, &ctx),
        Err(e) => HttpError::from_corelamo(CorelamoError::from(e), &ctx).into_response(),
    }
}

pub(crate) async fn set_config_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::SetConfig) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }
    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    //parse first - TOML parsing needs no database
    let options: DatabaseOptions = match toml::from_str(&body) {
        Ok(o) => o,
        Err(e) => {
            return HttpError::from_corelamo(CorelamoError::from(e), &ctx).into_response();
        }
    };

    let (handle, _gate) = match state.db_write(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    match handle
        .set_options_all(options, principal.id.0.clone())
        .await
    {
        Ok(_) => HttpOk::new(format!("config updated for '{db_name}'"), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(
            CorelamoError::Internal(format!("failed to update config for '{db_name}': {e}")),
            &ctx,
        )
        .into_response(),
    }
}

pub(crate) async fn list_databases_handler(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::ListDatabases) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let slots = state.databases.list();

    let entries = tokio::task::spawn_blocking(move || {
        slots
            .into_iter()
            .filter_map(|slot| {
                if slot.state() != SlotState::Live {
                    return None;
                }
                let manager = slot.manager().ok()?;
                Some(json!({ "name": slot.name.clone(), "running": manager.all_running() }))
            })
            .collect::<Vec<_>>()
    })
    .await;

    let entries = match entries {
        Ok(e) => e,
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("list task panicked: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    HttpOk::with_data(
        format!("{} database(s)", entries.len()),
        json!({ "databases": entries }),
        &ctx,
    )
    .into_response()
}

#[timed(backup)]
pub(crate) async fn backup_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::BackupFull) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    if let Err(e) = handle.try_start_backup() {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    tokio::spawn(async move {
        let _gate = gate;
        let result = handle.backup_full(principal.id.0.clone()).await;
        handle.finish_backup(result.is_ok());
    });

    HttpOk::new(format!("backup started for '{db_name}'"), &ctx).into_response()
}

#[timed(restore)]
pub(crate) async fn backup_restore_handler(
    State(state): State<AppState>,
    Path((db_name, backup_id)): Path<(String, String)>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    let id_for_log = backup_id.clone();
    let name_for_log = db_name.clone();

    if let Err(e) = check_permission(&state, &principal, Permission::Restore) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, _gate) = match state.db_write(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    if !handle.has_backup(&backup_id) {
        return HttpError::from_corelamo(
            CorelamoError::NotFound(format!("Backup with id '{id_for_log}' does not exist")),
            &ctx,
        )
        .into_response();
    }

    if let Err(e) = handle.try_start_restore() {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let result = handle
        .restore_backup(&backup_id, principal.id.0.clone())
        .await;
    handle.finish_restore(result.is_ok());

    match result {
        Ok(()) => {
            HttpOk::new(format!("restore completed for '{name_for_log}'"), &ctx).into_response()
        }
        Err(e) => HttpError::from_corelamo(
            CorelamoError::FailedToEx(format!("restore failed for '{name_for_log}': {e}")),
            &ctx,
        )
        .into_response(),
    }
}

#[timed(backup)]
pub(crate) async fn backup_incremental_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    let log = slog_scope::logger().new(o!("component" => "handlers"));
    if let Err(e) = check_permission(&state, &principal, Permission::BackupIncremental) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let backups = match handle.list_backups() {
        Ok(b) => b,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    if !backups.iter().any(|b| b.backup_type == BackupType::Full) {
        return HttpError::from_corelamo(
            CorelamoError::InvalidData("no full backup exists to base an increment on".into()),
            &ctx,
        )
        .into_response();
    }

    if let Err(e) = handle.try_start_backup() {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }
    let value = db_name.clone();
    tokio::spawn(async move {
        // same as the full backup: the guard has to outlive this request
        let _gate = gate;
        let result = handle.backup_incremental(principal.id.0.clone()).await;
        handle.finish_backup(result.is_ok());
        if let Err(e) = &result {
            error!(log, "incremental backup failed"; "database" => &value, "error" => %e);
        }
    });
    HttpOk::new(format!("incremental backup started for '{db_name}'"), &ctx).into_response()
}

#[timed(backup)]
pub(crate) async fn list_backups_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::ListBackups) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let backups = match handle.list_backups() {
        Ok(b) => b,
        Err(e) => {
            return HttpError::from_corelamo(CorelamoError::Internal(e.to_string()), &ctx)
                .into_response();
        }
    };

    HttpOk::with_data(
        format!("{} backup(s) for '{db_name}'", backups.len()),
        json!({ "backups": backups }),
        &ctx,
    )
    .into_response()
}

#[timed(backup)]
pub(crate) async fn backup_delete_handler(
    State(state): State<AppState>,
    Path((db_name, backup_id)): Path<(String, String)>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::DeleteBackup) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    if !handle.has_backup(&backup_id) {
        return HttpError::from_corelamo(
            CorelamoError::NotFound(format!("Backup with id '{backup_id}' does not exist")),
            &ctx,
        )
        .into_response();
    }

    if let Err(e) = handle
        .delete_backup(backup_id.clone(), principal.id.0.clone())
        .await
    {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    HttpOk::new(
        format!("backup '{backup_id}' deleted for '{db_name}'"),
        &ctx,
    )
    .into_response()
}

#[timed(database_lifecycle)]
pub(crate) async fn rename_database_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::RenameDatabase) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let body = match require_body(&body) {
        Ok(b) => b,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };
    let mut buf = body.as_bytes().to_vec();
    let req: RenameDatabaseRequest = match simd_json::from_slice(&mut buf) {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::InvalidData(format!("invalid rename-database request: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    // both names are registry keys and directory names, so both are folded
    let db_name = normalize_db_name(&db_name);
    let new_name = normalize_db_name(&req.name);

    if let Err(e) = validate_db_name(&new_name) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }
    if new_name == db_name {
        return HttpError::from_corelamo(
            CorelamoError::InvalidData(
                "new name is the same as the current name (names are case-insensitive)".to_string(),
            ),
            &ctx,
        )
        .into_response();
    }

    let log = slog_scope::logger().new(o!("component" => "handlers"));

    let slot = match state.databases.get(&db_name) {
        Ok(slot) => slot,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    // one lifecycle operation per database; released on every return path
    let _gate = slot.gate.write().await;

    let manager = match slot.manager() {
        Ok(manager) => manager,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    if manager.all_running() {
        return HttpError::from_corelamo(
            CorelamoError::Conflict(format!(
                "database '{db_name}' must be stopped before renaming; call stop-database first"
            )),
            &ctx,
        )
        .into_response();
    }

    // Claim the new name before anything is shut down: this refuses a name another slot
    // holds and a directory that only exists on disk, and the claimed slot is what makes a
    // concurrent create for the same name fail instead of racing this rename.
    let new_slot = match state.databases.reserve(&new_name) {
        Ok(slot) => slot,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    slot.set_state(SlotState::Renaming);

    // flush + close; by &self, so on failure the old slot keeps its manager
    let db = Arc::clone(&manager);
    let shut = tokio::task::spawn_blocking(move || db.shutdown()).await;
    let shut_err = match shut {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(CorelamoError::Internal(format!(
            "failed to shut down '{db_name}' before renaming: {e}"
        ))),
        Err(e) => Some(CorelamoError::Internal(format!(
            "shutdown task panicked: {e}"
        ))),
    };
    if let Some(e) = shut_err {
        state.databases.remove(&new_name);
        slot.set_state(SlotState::Live);
        error!(log, "rename-database failed"; "old_name" => %db_name, "new_name" => %new_name, "error" => %e);
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    // Move the directory, then reopen it. `false` means "do not boot": the database has to
    // still be stopped when it is handed back, and load() honours that.
    let old_path = slot.dir.clone();
    let new_path = new_slot.dir.clone();
    let (old_task, new_task) = (old_path.clone(), new_path.clone());
    let (old_label, new_label) = (db_name.clone(), new_name.clone());

    let result = tokio::task::spawn_blocking(move || -> Result<ShardManager, CorelamoError> {
        std::fs::rename(&old_task, &new_task).map_err(|e| {
            CorelamoError::Internal(format!(
                "failed to rename '{old_label}' to '{new_label}' on disk: {e}"
            ))
        })?;
        ShardManager::load(
            new_task.clone(),
            false,
            crate::corelamo_settings::synonym_registry(),
            crate::corelamo_settings::model_registry()
        )
        .map_err(|e| {
            // it moved but does not open: move it back so the old name keeps working
            let rolled_back = std::fs::rename(&new_task, &old_task).is_ok();
            CorelamoError::Internal(if rolled_back {
                format!(
                    "renamed on disk but failed to reopen '{new_label}': {e}; the rename was rolled back"
                )
            } else {
                format!(
                    "renamed on disk but failed to reopen '{new_label}': {e}, and rolling back \
                     failed; the data is at its new path and will be picked up on the next server restart"
                )
            })
        })
    })
    .await;

    match result {
        Ok(Ok(reopened)) => {
            let reopened = Arc::new(reopened);
            reopened.start_backup_scheduler();
            // the new name is live before the old slot goes away, so the database is never
            // absent from the registry
            new_slot.set_live(Arc::clone(&reopened));
            state.databases.remove(&db_name);
            info!(log, "database renamed"; "old_name" => %db_name, "new_name" => %new_name);
            HttpOk::new(
                format!("database '{db_name}' renamed to '{new_name}'"),
                &ctx,
            )
            .into_response()
        }
        Ok(Err(e)) => {
            state.databases.remove(&new_name);
            if old_path.exists() && !new_path.exists() {
                // the rename (or its rollback) left the directory at the old path, so the
                // slot keeps its stopped manager
                slot.set_state(SlotState::Live);
            } else {
                state.databases.remove(&db_name);
            }
            error!(log, "rename-database failed"; "old_name" => %db_name, "new_name" => %new_name, "error" => %e);
            HttpError::from_corelamo(e, &ctx).into_response()
        }
        Err(e) => {
            state.databases.remove(&new_name);
            if old_path.exists() && !new_path.exists() {
                slot.set_state(SlotState::Live);
            } else {
                state.databases.remove(&db_name);
            }
            error!(log, "rename-database task panicked"; "old_name" => %db_name, "new_name" => %new_name, "error" => %e);
            HttpError::from_corelamo(
                CorelamoError::Internal(format!("rename task panicked: {e}")),
                &ctx,
            )
            .into_response()
        }
    }
}

pub(crate) async fn timings_handler(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::Status) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (categories, file) = if body.trim().is_empty() {
        (None, None)
    } else {
        let mut buf = body.trim().as_bytes().to_vec();
        match simd_json::from_slice::<TimingsRequest>(&mut buf) {
            Ok(req) => (req.categories, req.file),
            Err(e) => {
                return HttpError::from_corelamo(
                    CorelamoError::InvalidData(format!("invalid timings request: {e}")),
                    &ctx,
                )
                .into_response();
            }
        }
    };

    let categories = categories.unwrap_or_default();
    let report = core_timing::report_filtered(&categories, file.as_deref());

    HttpOk::raw(StatusCode::OK, "text/plain", report, &ctx)
}

#[timed(disk_usage)]
pub(crate) async fn disk_usage_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::Status) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let slot = match state.databases.get(&db_name) {
        Ok(slot) => slot,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    // a delete or rename moves this directory out from under the walk
    let _gate = slot.read().await;

    // the slot knows the real directory, which keeps whatever case it was created with
    let db_root = slot.dir.clone();
    let name_for_task = slot.name.clone();

    let result =
        tokio::task::spawn_blocking(move || compute_disk_usage(&db_root, &name_for_task)).await;

    let usage = match result {
        Ok(Ok(u)) => u,
        Ok(Err(e)) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("disk usage task panicked: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    HttpOk::with_data(format!("disk usage for '{db_name}'"), usage, &ctx).into_response()
}

pub(crate) async fn get_dictionary_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::GetConfig) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let (handle, _gate) = match state.db_read(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    HttpOk::raw(
        StatusCode::OK,
        "application/toml",
        handle.dictionary_text(),
        &ctx,
    )
}

pub(crate) async fn set_dictionary_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::SetConfig) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }
    //parse first: a TOML error needs no database
    let document = match DictionaryDocument::parse(&db_name, &body) {
        Ok(d) => d,
        Err(e) => return HttpError::from_corelamo(CorelamoError::from(e), &ctx).into_response(),
    };

    let (handle, _gate) = match state.db_write(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    match handle.set_dictionary(document).await {
        Ok(()) => HttpOk::new(format!("dictionary updated for '{db_name}'"), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub(crate) async fn reset_dictionary_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::SetConfig) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }
    let (handle, _gate) = match state.db_write(&db_name).await {
        Ok(x) => x,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    match handle.reload_dictionary().await {
        Ok(()) => HttpOk::new(format!("dictionary reloaded for '{db_name}'"), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}
