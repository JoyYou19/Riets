//TODO: axum accepts <Response, Error>
//so the code cood look like: state.lookup(&db_name)?

use axum::{
    Extension,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use core_auth::Principal;
use core_core::{
    CorelamoDatabase, DatabaseOptions,
    command_reponse_definitions::{
        Command, DeleteCommand, LoginResponse, RetrieveCommand, RetrieveResponse, SearchCommand,
        SearchResponse,
    },
};

use crate::{
    AppState,
    db_actor::{self, DbHandle},
    doctypes,
    http_response::{BatchOutcome, HttpError, HttpOk},
    middleware::RequestContext,
};
use core_protocol::errors::{CorelamoError, DocFailure, FailReason};
use core_storage::search_database::DatabasePowerButtonOutcome;
use serde_json::json;
use std::collections::BTreeMap;

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
                CorelamoError::Internal(format!("invalid login request:{e}")),
                &ctx,
            )
            .into_response();
        }
    };
    let Ok(auth) = state.auth.read() else {
        return HttpError::from_corelamo(
            CorelamoError::Internal("auth service lock poisoned".to_string()),
            &ctx,
        )
        .into_response();
    };
    match auth.login(&req.username, &req.password) {
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

//TODO: total_hits: xxx kkadu
pub async fn search_handler(
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

    let command: SearchCommand = match SearchCommand::parse(&body, ctx.format) {
        Ok(cmd) => cmd,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let query = command.query.clone();

    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let hits = match handle.search(command).await {
        Ok(hits) => hits,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

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

//TODO: cant really see the id if auto-increment :(
pub async fn retrieve_handler(
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

    let results = match handle.retrieve(command.ids).await {
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

pub async fn insert_handler(
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

    match handle.is_running().await {
        Ok(true) => {}
        Ok(false) => {
            return HttpError::from_corelamo(
                CorelamoError::DatabaseNotRunning(format!("database {db_name} is not running")),
                &ctx,
            )
            .into_response();
        }
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    }

    let policy = match handle.get_policy().await {
        Ok(p) => p,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

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

    let report = match handle.insert(outcome.docs).await {
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

pub async fn delete_document_handler(
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

    let report = match handle.delete(command.ids).await {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
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

    let policy = match handle.get_policy().await {
        Ok(p) => p,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let format = ctx.format;
    let parsed =
        tokio::task::spawn_blocking(move || doctypes::parse_documents(&body, format, &policy))
            .await;

    let parse_outcome = match parsed {
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

    let doc_indices = parse_outcome.indices;
    let parse_failures = parse_outcome.failures;

    let report = match handle.replace(parse_outcome.docs).await {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
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
    let policy = match handle.get_policy().await {
        Ok(p) => p,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let format = ctx.format;
    let parsed =
        tokio::task::spawn_blocking(move || doctypes::parse_documents(&body, format, &policy))
            .await;

    let parse_outcome = match parsed {
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

    let doc_indices = parse_outcome.indices;
    let parse_failures = parse_outcome.failures;

    let results = match handle.upsert(parse_outcome.docs).await {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let mut outcome = BatchOutcome::new("upserted", StatusCode::INTERNAL_SERVER_ERROR);
    outcome.fail_many(parse_failures);

    // actor indexes into the parsed vec; map back to the client's input array
    for (index, id, result) in results {
        match result {
            Ok(()) => outcome.succeed(),
            Err(e) => outcome.fail_doc(DocFailure::new(
                doc_indices.get(index).copied(),
                Some(id),
                FailReason::Internal(e.message()),
            )),
        }
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
) -> Response {
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
        CorelamoDatabase::create(&db_path, DatabaseOptions::default())
    })
    .await;

    let db = match created {
        Ok(Ok(db)) => db,
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

    let (handle, join) = db_actor::spawn_db_actor(db, db_name.clone());
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
        if dbs.contains_key(&db_name) {
            drop(dbs);
            drop(handle);
            let _ = join.join();
            return HttpError::from_corelamo(
                CorelamoError::AlreadyExists(format!("database '{db_name}' already exists")),
                &ctx,
            )
            .into_response();
        }
        dbs.insert(db_name.clone(), handle);
    }

    HttpOk::with_status(
        StatusCode::CREATED,
        format!("database '{db_name}' created"),
        &ctx,
    )
    .into_response()
}

pub async fn start_database_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    match handle.start().await {
        Ok(DatabasePowerButtonOutcome::Changed) => {
            HttpOk::new(format!("database '{db_name}' started"), &ctx).into_response()
        }
        Ok(DatabasePowerButtonOutcome::Nochange) => {
            HttpOk::new(format!("database '{db_name}' is already running"), &ctx).into_response()
        }
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub async fn stop_database_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    match handle.stop().await {
        Ok(DatabasePowerButtonOutcome::Changed) => {
            HttpOk::new(format!("database '{db_name}' stopped"), &ctx).into_response()
        }
        Ok(DatabasePowerButtonOutcome::Nochange) => {
            HttpOk::new(format!("database '{db_name}' is already stopped"), &ctx).into_response()
        }
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub async fn delete_database_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let handle = {
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
    }; //guard dropped

    if let Err(e) = handle.shutdown().await {
        return HttpError::from_corelamo(
            CorelamoError::Internal(format!("failed to shutdown database '{db_name}': {e}")),
            &ctx,
        )
        .into_response();
    }
    drop(handle); //last Sender gone

    let db_path = state.databases_dir.join(&db_name);
    let removed = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&db_path)).await;

    match removed {
        Ok(Ok(())) => HttpOk::new(format!("database '{db_name}' deleted"), &ctx).into_response(),
        Ok(Err(e)) => HttpError::from_corelamo(
            CorelamoError::Internal(format!(
                "removed from memory but failed to delete '{db_name}' from disk: {e}"
            )),
            &ctx,
        )
        .into_response(),
        Err(e) => HttpError::from_corelamo(
            CorelamoError::Internal(format!("delete task panicked: {e}")),
            &ctx,
        )
        .into_response(),
    }
}

pub async fn stats_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };
    let stats = handle.stats().await;
    match stats {
        Ok(stats) => {
            let indexing = &stats.indexing;
            let reindexing = &stats.reindexing;
            HttpOk::with_data(
                format!("stats for '{db_name}'"),
                json!({
                    "document_count": stats.document_count,
                    "segment_count": stats.segment_count,
                    "background_compaction_enabled": stats.background_compaction_enabled,
                    "metrics": {
                          "search_requests": stats.metrics.search_requests,
                          "search_errors": stats.metrics.search_errors,
                          "indexing_requests": stats.metrics.indexing_requests,
                          "indexing_errors": stats.metrics.indexing_errors,
                         },
                    "indexed": {
                        "total_documents_indexed": indexing.total_documents_indexed,
                        "total_documents_deleted": indexing.total_documents_deleted,
                        "segments_written": indexing.segments_written,
                        "compactions_completed": indexing.compactions_completed,
                        "memtable_term_count": indexing.memtable_term_count,
                        "segment_count": indexing.segment_count,
                    },
                    "reindexing": {
                        "status": reindexing.status,
                        "progress": reindexing.progress,
                        "eta_seconds": reindexing.eta_seconds
                    }
                }),
                &ctx,
            )
            .into_response()
        }
        //TODO: upate once stats gets updated with errors
        Err(e) => HttpError::from_corelamo(
            CorelamoError::Internal(format!("failed to get stats: {e}")),
            &ctx,
        )
        .into_response(),
    }
}

pub async fn reindex_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    match handle.reindex().await {
        Ok(()) => HttpOk::new(format!("reindex complete for '{db_name}'"), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

// policy is always TOML
// sending raw since it shouldnt be encoded in json/xml
pub async fn get_policy_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    let policy = match handle.get_policy().await {
        Ok(p) => p,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    match doctypes::serialize_policy(&policy) {
        Ok(output) => HttpOk::raw(StatusCode::OK, "application/toml", output, &ctx),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub async fn set_policy_handler(
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

    match handle.set_policy(policy).await {
        Ok(()) => HttpOk::new(format!("policy updated for '{db_name}'"), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub async fn get_config_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };
    let options = handle.options().await;

    match toml::to_string_pretty(&options) {
        Ok(output) => HttpOk::raw(StatusCode::OK, "application/toml", output, &ctx),
        Err(e) => HttpError::from_corelamo(CorelamoError::from(e), &ctx).into_response(),
    }
}

pub async fn set_config_handler(
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

    match handle.set_options(options).await {
        Ok(was_running) => {
            let note = if was_running {
                " (restart required for start-time settings to take effect)"
            } else {
                ""
            };
            HttpOk::new(format!("config updated for '{db_name}'{note}"), &ctx).into_response()
        }
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub async fn restart_database_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let handle = match state.lookup(&db_name) {
        Ok(h) => h,
        Err(e) => {
            return HttpError::from_corelamo(e, &ctx).into_response();
        }
    };

    match handle.restart().await {
        Ok(()) => {
            HttpOk::new(format!("database '{db_name}' succesfuly restarted"), &ctx).into_response()
        }
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub async fn list_databases_handler(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let handles: Vec<(String, DbHandle)> = {
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
            .map(|(name, h)| (name.clone(), h.clone()))
            .collect()
    };

    let count = handles.len();

    let mut entries = Vec::with_capacity(count);
    for (name, handle) in handles {
        let running = handle.is_running().await.unwrap_or(false);
        entries.push(json!({ "name": name, "running": running }));
    }

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
