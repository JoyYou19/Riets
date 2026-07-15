use std::{
    collections::{BTreeMap, HashMap},
    sync::{RwLockReadGuard, RwLockWriteGuard},
};

use axum::{
    Extension,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};

use core_core::{
    CorelamoDatabase,
    command_reponse_definitions::{
        Command, DeleteCommand, LoginResponse, RetrieveCommand, RetrieveResponse, SearchCommand,
        SearchResponse,
    },
};
use core_protocol::errors::CorelamoError;
use serde_json::json;

use crate::{
    AppState,
    database_helpers::{self},
    doctypes,
    http_response::{BatchOutcome, HttpError, HttpOk},
    middleware::RequestContext,
};

use core_auth::Principal;
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

//helpers
fn get_db_read<'a>(
    databases: &'a RwLockReadGuard<HashMap<String, CorelamoDatabase>>,
    db_name: &str,
) -> Result<&'a CorelamoDatabase, CorelamoError> {
    databases
        .get(db_name)
        .ok_or_else(|| CorelamoError::NotFound(format!("database '{db_name}' not found")))
}

fn get_db_write<'a>(
    databases: &'a mut RwLockWriteGuard<HashMap<String, CorelamoDatabase>>,
    db_name: &str,
) -> Result<&'a mut CorelamoDatabase, CorelamoError> {
    databases
        .get_mut(db_name)
        .ok_or_else(|| CorelamoError::NotFound(format!("database '{db_name}' not found")))
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
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
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
            CorelamoError::Unauthorized("Invalid username or password".to_string()),
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
        Ok(b) => b,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let command: SearchCommand = match SearchCommand::parse(body, ctx.format) {
        Ok(cmd) => cmd,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let mut databases = state.databases.write().unwrap();
    let db = match get_db_write(&mut databases, &db_name) {
        Ok(db) => db,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let hits = match db.search(&command) {
        Ok(hits) => hits,
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("search failed: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    let hit_count = hits.len();
    let projected: Vec<(String, BTreeMap<String, String>)> = hits
        .into_iter()
        .map(|hit| (hit.external_id, hit.fields))
        .collect();

    let resp = match SearchResponse::from_hits(projected) {
        Ok(r) => r,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };
    HttpOk::with_response(
        format!("{hit_count} hit(s) for '{}'", &command.query),
        resp,
        &ctx,
    )
    .into_response()
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
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let command = match RetrieveCommand::parse(&body, ctx.format) {
        Ok(cmd) => cmd,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };
    let ids = command.ids;

    let mut databases = state.databases.write().unwrap();
    let db = match get_db_write(&mut databases, &db_name) {
        Ok(db) => db,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let mut docs = Vec::new();
    let mut not_found_ids: Vec<String> = Vec::new();

    for id in &ids {
        match db.get_document(id) {
            Ok(Some(doc)) => docs.push(doc),
            Ok(None) => not_found_ids.push(id.clone()),
            //TODO:  update error handling once the get_document gets updated
            Err(e) => {
                return HttpError::from_corelamo(
                    CorelamoError::Internal(format!("failed to get document '{id}': {e}")),
                    &ctx,
                )
                .into_response();
            }
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

//TODO: padomat kaa smuki paradit ne tikai duplicate id bet arii kkadu invalid json
//TODO: multi-threaded parsing to DocInput
pub async fn insert_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    body: String,
) -> Response {
    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let mut databases = state.databases.write().unwrap();
    let db = match get_db_write(&mut databases, &db_name) {
        Ok(db) => db,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let input_docs = match doctypes::parse_documents(&body, ctx.format, db.policy()) {
        Ok(d) => d,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    // storage assigns auto-increment ids, skips duplicates, and reports both back
    let report = match db.put_documents_parallel(input_docs) {
        Ok(r) => r,
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("insert failed: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    let mut outcome = BatchOutcome::new("inserted", StatusCode::CONFLICT);
    outcome.succeed_many(report.inserted);
    for id in report.duplicates {
        outcome.fail(id, 409, "DUPLICATE ID");
    }

    let title = format!("inserted {} into '{db_name}'", report.inserted);
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
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let command = match DeleteCommand::parse(&body, ctx.format) {
        Ok(cmd) => cmd,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let mut databases = state.databases.write().unwrap();
    let db = match get_db_write(&mut databases, &db_name) {
        Ok(db) => db,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let mut outcome = BatchOutcome::new("deleted", StatusCode::NOT_FOUND);

    for id in command.ids {
        match db.get_document(&id) {
            Ok(Some(_)) => match db.delete_document(&id) {
                Ok(_) => outcome.succeed(),
                Err(e) => {
                    return HttpError::from_corelamo(
                        CorelamoError::Internal(format!("failed to delete '{id}': {e}")),
                        &ctx,
                    )
                    .into_response();
                }
            },
            Ok(None) => outcome.fail(id, 404, "NOT FOUND"),
            Err(e) => {
                return HttpError::from_corelamo(
                    CorelamoError::Internal(format!("failed to lookup '{id}': {e}")),
                    &ctx,
                )
                .into_response();
            }
        }
    }

    let title = format!(
        "deleted {} document(s) from '{db_name}', {} not found",
        outcome.succeeded_count(),
        outcome.failed_count()
    );

    outcome
        .into_ok(StatusCode::OK, title, &db_name, &ctx)
        .into_response()
}

//TODO: valtera update has upsert capabilities, we could make another command for upsert document
//too imagine
pub async fn update_document_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    body: String,
) -> Response {
    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let mut databases = state.databases.write().unwrap();
    let db = match get_db_write(&mut databases, &db_name) {
        Ok(db) => db,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let input_docs = match doctypes::parse_documents(&body, ctx.format, db.policy()) {
        Ok(d) => d,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let mut not_found = Vec::new();
    let mut updated_count = 0;

    for doc in input_docs {
        match db.get_document(&doc.external_id) {
            Ok(Some(_)) => {
                updated_count += 1;
                //FIX: im not sure if this unwrap is entirely safe long term
                db.update_document(doc).unwrap();
            }
            Ok(None) => not_found.push(doc.external_id),
            Err(e) => {
                return HttpError::from_corelamo(
                    CorelamoError::Internal(format!("existence check failed: {e}")),
                    &ctx,
                )
                .into_response();
            }
        }
    }

    let mut outcome = BatchOutcome::new("updated", StatusCode::NOT_FOUND);
    outcome.succeed_many(updated_count as u32);
    for id in not_found {
        outcome.fail(id, 404, "NOT FOUND");
    }
    let title = format!("updated {updated_count} in '{db_name}'");
    outcome
        .into_ok(StatusCode::OK, title, &db_name, &ctx)
        .into_response()
}

pub async fn create_database_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let databases_read = state.databases.read().unwrap();
    match database_helpers::create_database(db_name.clone(), &state.databases_dir, &databases_read)
    {
        Ok(db) => {
            drop(databases_read);
            state.databases.write().unwrap().insert(db_name.clone(), db);
            HttpOk::with_status(
                StatusCode::CREATED,
                format!("database '{db_name}' created"),
                &ctx,
            )
            .into_response()
        }
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}

pub async fn delete_detabase_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let db_path = state.databases_dir.join(&db_name);
    let mut databases_write = state.databases.write().unwrap();

    if !databases_write.contains_key(&db_name) {
        return HttpError::from_corelamo(
            CorelamoError::NotFound(format!("database '{db_name}' not found")),
            &ctx,
        )
        .into_response();
    }

    if let Some(db) = databases_write.remove(&db_name) {
        if let Err(e) = db.shutdown() {
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("failed to shutdown database '{db_name}': {e}")),
                &ctx,
            )
            .into_response();
        }
    }

    if let Err(e) = std::fs::remove_dir_all(&db_path) {
        return HttpError::from_corelamo(
            CorelamoError::Internal(format!(
                "removed from memory but failed to delete '{db_name}' from disk: {e}"
            )),
            &ctx,
        )
        .into_response();
    }

    HttpOk::new(format!("database '{db_name}' deleted"), &ctx).into_response()
}

pub async fn stats_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let databases = state.databases.read().unwrap();
    let db = match get_db_read(&databases, &db_name) {
        Ok(db) => db,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    match db.stats() {
        Ok(stats) => HttpOk::with_data(
            format!("stats for '{db_name}'"),
            json!({
                "document_count": stats.document_count,
                "segment_count": stats.segment_count,
                "background_compaction_enabled": stats.background_compaction_enabled,
            }),
            &ctx,
        )
        .into_response(),
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
    let mut databases = state.databases.write().unwrap();
    let db = match get_db_write(&mut databases, &db_name) {
        Ok(db) => db,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    match db.reindex() {
        Ok(_) => HttpOk::new(format!("reindex complete for '{db_name}'"), &ctx).into_response(),
        //TODO:  update once reindex errors updated
        Err(e) => HttpError::from_corelamo(
            CorelamoError::Internal(format!("reindex failed: {e}")),
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
) -> Response {
    let databases = state.databases.read().unwrap();
    let db = match get_db_read(&databases, &db_name) {
        Ok(db) => db,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    match doctypes::serialize_policy(db.policy()) {
        Ok(output) => HttpOk::raw(StatusCode::OK, "application/toml", output, &ctx),
        Err(e) => HttpError::from_corelamo(CorelamoError::from(e), &ctx).into_response(),
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
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let mut databases = state.databases.write().unwrap();
    let db = match get_db_write(&mut databases, &db_name) {
        Ok(db) => db,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let policy = match doctypes::parse_policy(&body) {
        Ok(p) => p,
        Err(e) => return HttpError::from_corelamo(CorelamoError::from(e), &ctx).into_response(),
    };

    match db.set_policy(policy) {
        Ok(_) => HttpOk::new(format!("policy updated for '{db_name}'"), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(CorelamoError::from(e), &ctx).into_response(),
    }
}

pub async fn list_databases_handler(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
) -> Response {
    let databases = state.databases.read().unwrap();
    let names: Vec<&String> = databases.keys().collect();
    let count = names.len();

    HttpOk::with_data(
        format!("{count} database(s) loaded"),
        json!({ "databases": names }),
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
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
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
    let Ok(mut auth) = state.auth.write() else {
        return HttpError::from_corelamo(
            CorelamoError::Internal("auth service lock poisoned".to_string()),
            &ctx,
        )
        .into_response();
    };

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
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
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

    let Ok(mut auth) = state.auth.write() else {
        return HttpError::from_corelamo(
            CorelamoError::Internal("auth service lock poisoned".to_string()),
            &ctx,
        )
        .into_response();
    };

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
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
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

    let Ok(mut auth) = state.auth.write() else {
        return HttpError::from_corelamo(
            CorelamoError::Internal("auth service lock poisoned".to_string()),
            &ctx,
        )
        .into_response();
    };

    match auth.update_user_roles(&principal, &username, req.roles) {
        Ok(()) => HttpOk::new(format!("roles updated for '{}'", username), &ctx).into_response(),
        Err(e) => HttpError::from_corelamo(e, &ctx).into_response(),
    }
}