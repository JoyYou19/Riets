//TODO: axum accepts <Response, Error>
//so the code cood look like: state.lookup(&db_name)?

use axum::{
    Extension,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use core_auth::{Permission, Principal};

use core_core::{DatabaseOptions, shard_manager::ShardManager, shard_worker::ShardCmd::BackupFull};
use slog::{error, info, o};

use crate::{
    AppState, doctypes,
    http_response::{BatchOutcome, HttpError, HttpOk},
    middleware::RequestContext,
};
use core_protocol::{
    command_reponse_definitions::{
        Command, DeleteCommand, GetLogsRequest, LoginResponse, LookupCommand, RetrieveCommand,
        RetrieveResponse, SearchCommand, SearchResponse,
    },
    errors::CorelamoError,
};
use serde_json::json;
use std::{collections::BTreeMap, path::Component::RootDir, sync::Arc};

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
pub async fn login_handler(
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
    let req: LoginRequest = match serde_json::from_str(body) {
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

// //TODO: total_hits: xxx kkadu
pub async fn search_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::Search) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }
    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
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
    let hits = match manager.search(&command) {
        Ok(hits) => hits,
        Err(e) => {
            handle.record_search(true, start.elapsed());
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("search task panicked: {e}")),
                &ctx,
            )
            .into_response();
        }
    };
    handle.record_search(false, start.elapsed());
    let hit_count = hits.len();
    let projected: Vec<(String, BTreeMap<String, String>)> = hits
        .into_iter()
        .map(|hit| (hit.external_id, hit.fields))
        .collect();

    let resp = match SearchResponse::from_hits(projected) {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    HttpOk::with_response(format!("{hit_count} hit(s) for '{query}'"), resp, &ctx).into_response()
}

pub async fn lookup_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(_principal): Extension<Principal>,
    body: String,
) -> Response {
    let manager = match state.lookup(&db_name) {
        Ok(m) => m,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };
    if !manager.all_running() {
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

    let result = manager.lookup(&command);

    let resp = match result {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let hit_count = resp.docs.len();
    HttpOk::with_response(format!("looked up {hit_count} document(s)"), resp, &ctx).into_response()
}

pub async fn insert_handler(
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

    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
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
        tokio::task::spawn_blocking(move || doctypes::parse_documents(&body, format, &policy))
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
    let report = match manager.insert(outcome.docs).await {
        Ok(r) => r,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
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

pub async fn retrieve_handler(
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

    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
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
    let results = match manager.retrieve(ids) {
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

    let (documents, skipped_ids) = doctypes::convert_from_storage(&docs, ctx.format);

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

pub async fn start_database_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let manager = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    match manager.start().await {
        Ok(()) => HttpOk::new(format!("database '{db_name}' started"), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub async fn stop_database_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let manager = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    match manager.stop().await {
        Ok(()) => HttpOk::new(format!("database '{db_name}' stopped"), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub async fn restart_database_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let manager = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    match manager.restart().await {
        Ok(()) => {
            HttpOk::new(format!("database '{db_name}' succesfuly restarted"), &ctx).into_response()
        }
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub async fn clear_database_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::Delete) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let manager = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    match manager.clear_all().await {
        Ok(_) => {
            HttpOk::new(
                format!("database: {db_name}, is cleared of data and index"),
                &ctx,
            )
            .into_response()
        }

        Err(e) => {
            HttpError::from_corelamo(e, &ctx).into_response()
        }
    }
}

pub async fn get_logs_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::GetLogs) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let date = if body.trim().is_empty() {
        None
    } else {
        //insane smukais porno lai dabutu date: "" aaraa
        match serde_json::from_str::<GetLogsRequest>(body.trim()) {
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

pub async fn clear_logs_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::ClearLogs) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    match handle.clear_logs() {
        Ok(_) => HttpOk::new(format!("Deleted logs for {db_name}"), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub async fn delete_document_handler(
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

    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
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

    let report = match manager.delete(ids).await {
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

pub async fn replace_document_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    body: String,
) -> Response {
    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
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
        tokio::task::spawn_blocking(move || doctypes::parse_documents(&body, format, &policy))
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
    let report = match manager.replace(outcome.docs).await {
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

pub async fn upsert_document_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    body: String,
) -> Response {
    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
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
        tokio::task::spawn_blocking(move || doctypes::parse_documents(&body, format, &policy))
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
    let report = match manager.upsert(outcome.docs).await {
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

pub async fn create_database_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::CreateDatabase) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }
    {
        let dbs = match state.databases.read() {
            Ok(g) => g,
            Err(_) => {
                return HttpError::from_corelamo(
                    CorelamoError::Internal("databases lock poisoned".into()),
                    &ctx,
                )
                .into_response();
            }
        };
        if dbs.contains_key(&db_name) {
            return HttpError::from_corelamo(
                CorelamoError::AlreadyExists(format!("database '{db_name}' already exists")),
                &ctx,
            )
            .into_response();
        }
    }
    let db_path = state.databases_dir.join(&db_name);

    let created = tokio::task::spawn_blocking(move || {
        ShardManager::create(db_path, 6, DatabaseOptions::default())
    })
    .await;

    let manager = match created {
        Ok(Ok(mgr)) => mgr,
        Ok(Err(e)) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("create task panicked: {e}")),
                &ctx,
            )
            .into_response();
        }
    };
    {
        let mut dbs = match state.databases.write() {
            Ok(g) => g,
            Err(_) => {
                return HttpError::from_corelamo(
                    CorelamoError::Internal("databases lock poisoned".into()),
                    &ctx,
                )
                .into_response();
            }
        };

        //safety check if someone somehow created a database between the two checks
        if dbs.contains_key(&db_name) {
            if let Err(e) = manager.shutdown() {
                error!(slog_scope::logger(), "failed to shut down redundant manager"; "db" => %db_name, "error" => %e);
            }
            return HttpError::from_corelamo(
                CorelamoError::AlreadyExists(format!("database '{db_name}' already exists")),
                &ctx,
            )
            .into_response();
        }
        dbs.insert(db_name.clone(), Arc::new(manager));
    }

    HttpOk::with_status(
        StatusCode::CREATED,
        format!("database '{db_name}' created"),
        &ctx,
    )
    .into_response()
}

pub async fn delete_database_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let log = slog_scope::logger().new(o!("components" => "handlers"));

    let manager = {
        let mut dbs = match state.databases.write() {
            Ok(g) => g,
            Err(_) => {
                return HttpError::from_corelamo(
                    CorelamoError::Internal("databases lock poisoned".into()),
                    &ctx,
                )
                .into_response();
            }
        };
        match dbs.remove(&db_name) {
            Some(h) => h,
            None => {
                return HttpError::from_corelamo(
                    CorelamoError::NotFound(format!("database '{db_name}' not found")),
                    &ctx,
                )
                .into_response();
            }
        }
    };

    let manager = match Arc::try_unwrap(manager) {
        Ok(mgr) => mgr,
        Err(still_shared) => {
            // put it back so the database stays reachable/usable
            if let Ok(mut dbs) = state.databases.write() {
                dbs.insert(db_name.clone(), still_shared);
            }
            return HttpError::from_corelamo(
                CorelamoError::Conflict(format!("database '{db_name}' is in use, try again")),
                &ctx,
            )
            .into_response();
        }
    };

    if let Err(e) = tokio::task::spawn_blocking(move || manager.shutdown()).await {
        error!(log, "delete: shutdown task panicked"; "db" => %db_name, "error" => %e);
    }

    let db_path = state.databases_dir.join(&db_name);
    let removed = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&db_path)).await;

    match removed {
        Ok(Ok(())) => {
            info!(log, "database deleted"; "name" => %db_name);
            HttpOk::new(format!("database '{db_name}' deleted"), &ctx).into_response()
        }
        Ok(Err(e)) => {
            error!(log, "database delete failed"; "name" => %db_name, "error" => %e);
            HttpError::from_corelamo(
                CorelamoError::Internal(format!(
                    "removed from memory but failed to delete '{db_name}' from disk: {e}"
                )),
                &ctx,
            )
            .into_response()
        }
        Err(e) => {
            error!(log, "database delete panicked"; "name" => %db_name, "error" => %e);
            HttpError::from_corelamo(
                CorelamoError::Internal(format!("delete task panicked: {e}")),
                &ctx,
            )
            .into_response()
        }
    }
}
pub async fn stats_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let manager = match state.lookup(&db_name) {
        Ok(m) => m,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    // atomics only: this never queues behind an fsync or a reindex commit
    let stats = manager.stats();
    let indexing = &stats.indexing;
    let reindexing = &stats.reindexing;
    let metrics = &stats.metrics;

    HttpOk::with_data(
        format!("stats for '{db_name}'"),
        json!({
            "background_compaction_enabled": stats.background_compaction_enabled,
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
            }
        }),
        &ctx,
    )
    .into_response()
}

pub async fn reindex_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let manager = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    // prepare flushes each shard, so keep it off the tokio workers
    let m = Arc::clone(&manager);
    match tokio::task::spawn_blocking(move || m.reindex()).await {
        Ok(Ok(())) => HttpOk::new(
            format!("reindex started for '{db_name}', poll /status for progress"),
            &ctx,
        )
        .into_response(),
        Ok(Err(e)) => HttpError::from_corelamo(e, &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(
            CorelamoError::Internal(format!("reindex task panicked: {e}")),
            &ctx,
        )
        .into_response(),
    }
}

// policy is always TOML
// sending raw since it shouldnt be encoded in json/xml
pub async fn get_policy_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::GetPolicy) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }
    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let policy = handle.policy();

    match doctypes::serialize_policy(&policy) {
        Ok(output) => HttpOk::raw(StatusCode::OK, "application/toml", output, &ctx),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub async fn set_policy_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::PostPolicy) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }
    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let policy = match doctypes::parse_policy(&body) {
        Ok(p) => p,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    match handle.set_policy_all(policy).await {
        Ok(()) => HttpOk::new(format!("policy updated for '{db_name}'"), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub async fn get_config_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(e) = check_permission(&state, &principal, Permission::GetConfig) {
        return HttpError::from_corelamo(e, &ctx).into_response();
    }

    let manager = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let options = manager.options();

    match toml::to_string_pretty(&options) {
        Ok(output) => HttpOk::raw(StatusCode::OK, "application/toml", output, &ctx),
        Err(e) => HttpError::from_corelamo(CorelamoError::from(e), &ctx).into_response(),
    }
}

pub async fn set_config_handler(
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

    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    match handle.set_options_all(options).await {
        Ok(_) => HttpOk::new(format!("config updated for '{db_name}'"), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(
            CorelamoError::Internal(format!("failed to update config for '{db_name}': {e}")),
            &ctx,
        )
        .into_response(),
    }
}

pub async fn list_databases_handler(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let managers: Vec<(String, Arc<ShardManager>)> = {
        let dbs = match state.databases.read() {
            Ok(g) => g,
            Err(_) => {
                return HttpError::from_corelamo(
                    CorelamoError::Internal("databases lock poisoned".into()),
                    &ctx,
                )
                .into_response();
            }
        };
        dbs.iter()
            .map(|(name, m)| (name.clone(), Arc::clone(m)))
            .collect()
    };

    let count = managers.len();

    let entries = tokio::task::spawn_blocking(move || {
        managers
            .into_iter()
            .map(|(name, manager)| {
                let running = manager.all_running();
                json!({ "name": name, "running": running })
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
        format!("{count} database(s)"),
        json!({ "databases": entries }),
        &ctx,
    )
    .into_response()
}

pub async fn create_user_handler(
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
    let req: CreateUserRequest = match serde_json::from_str(body) {
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
pub async fn delete_user_handler(
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

pub async fn update_user_password_handler(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    //varbut japieliek tas check permission
    let body = match require_body(&body) {
        Ok(b) => b,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };
    let req: UpdatePasswordRequest = match serde_json::from_str(body) {
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

pub async fn update_user_roles_handler(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
    body: String,
) -> Response {
    let body = match require_body(&body) {
        Ok(b) => b,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };
    let req: UpdateRolesRequest = match serde_json::from_str(body) {
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

pub async fn backup_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    // if let Err(e) = check_permission(&state, &principal, Permission::Backup) {
    //     return HttpError::from_corelamo(e, &ctx).into_response();
    // }

    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let name_for_log = db_name.clone();
    tokio::spawn(async move {
        match handle.backup_full().await {
            Ok(_manifests) => slog::info!(slog::Logger::root(slog::Discard, o!()), "backup completed"; "db" => %name_for_log), //parmainit
            Err(_e) => slog::error!(slog::Logger::root(slog::Discard, o!()), "backup failed"; "db" => %name_for_log),
        }
    });

    HttpOk::new(format!("backup started for '{db_name}'"), &ctx).into_response()
}

 pub async fn backup_restore_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    Extension(principal): Extension<Principal>,
) -> Response {
    // if let Err(e) = check_permission(&state, &principal, Permission::Backup) {
    //     return HttpError::from_corelamo(e, &ctx).into_response();
    // }

    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    match handle.restore_backup().await {
        Ok(()) => HttpOk::new(format!("restored '{db_name}' from latest backup"), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(
            CorelamoError::Internal(format!("restore failed for '{db_name}': {e}")),
            &ctx,
        )
        .into_response(),
    }
}
