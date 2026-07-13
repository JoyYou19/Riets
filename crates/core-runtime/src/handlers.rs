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
        Command, RetrieveCommand, RetrieveResponse, SearchCommand, SearchResponse,
    },
};
use core_protocol::errors::CorelamoError;
use serde_json::{Value, json};

use crate::{
    AppState,
    database_helpers::{self},
    doctypes,
    http_response::{BatchOutcome, HttpError, HttpOk},
    middleware::RequestContext,
};

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

    let input_docs = match doctypes::parse_documents(&body, ctx.format) {
        Ok(d) => d,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let mut databases = state.databases.write().unwrap();
    let db = match get_db_write(&mut databases, &db_name) {
        Ok(db) => db,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let mut to_insert = Vec::new();
    let mut duplicate_ids = Vec::new();

    for doc in input_docs {
        match db.get_document(&doc.external_id) {
            Ok(Some(_)) => duplicate_ids.push(doc.external_id),
            Ok(None) => to_insert.push(doc),
            Err(e) => {
                return HttpError::from_corelamo(
                    CorelamoError::Internal(format!("Duplicate key check failed: {e}")),
                    &ctx,
                )
                .into_response();
            }
        }
    }
    let inserted_count = to_insert.len();

    if to_insert.is_empty() {
        // Nothing to insert everything was a duplicate
        return HttpError::from_corelamo(
            CorelamoError::Conflict("Duplicate Primary ID(s)".to_string()),
            &ctx,
        )
        .into_response();
    }

    let inserted_ids: Vec<String> = to_insert.iter().map(|d| d.external_id.clone()).collect();

    match db.put_documents_parallel(to_insert) {
        Ok(_) => {
            let mut outcome = BatchOutcome::new();
            for id in inserted_ids {
                outcome.succeed(id, 201, "Inserted");
            }
            for id in duplicate_ids {
                outcome.fail(id, 409, "DUPLICATE ID");
            }
            let title = format!(
                "inserted {inserted_count}, skipped {} in '{db_name}'",
                outcome.failed_count()
            );
            outcome
                .into_ok(StatusCode::OK, title, &db_name, &ctx)
                .into_response()
        }
        Err(e) => HttpError::from_corelamo(
            CorelamoError::Internal("Insert failed for some reason".to_string()),
            &ctx,
        )
        .into_response(),
    }
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

    HttpError::from_corelamo(
        CorelamoError::NotFound("delete not implemented".to_string()),
        &ctx,
    )
    .into_response()
}

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

    let input_docs = match doctypes::parse_documents(&body, ctx.format) {
        Ok(d) => d,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let mut databases = state.databases.write().unwrap();
    let db = match get_db_write(&mut databases, &db_name) {
        Ok(db) => db,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let mut to_update = Vec::new();
    let mut not_found = Vec::new();

    for doc in input_docs {
        match db.get_document(&doc.external_id) {
            Ok(Some(_)) => to_update.push(doc),
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

    let updated_count = to_update.len();
    if to_update.is_empty() {
        return HttpError::from_corelamo(
            CorelamoError::NotFound("no matching document ID(s) to update".to_string()),
            &ctx,
        )
        .into_response();
    }

    let updated_ids: Vec<String> = to_update.iter().map(|d| d.external_id.clone()).collect();

    for doc in to_update {
        let id = doc.external_id.clone();
        if let Err(e) = db.update_document(doc) {
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("failed to update '{id}': {e}")),
                &ctx,
            )
            .into_response();
        }
    }

    if not_found.is_empty() {
        HttpOk::with_data_and_status(
            StatusCode::OK,
            format!("updated {updated_count} document(s) in '{db_name}'"),
            json!({
                "updated": updated_count,
                "database": db_name,
            }),
            &ctx,
        )
        .into_response()
    } else {
        let results: Vec<Value> = updated_ids
            .iter()
            .map(|id| json!({ "id": id, "status": 200, "result": "Updated" }))
            .chain(
                not_found
                    .iter()
                    .map(|id| json!({ "id": id, "status": 404, "result": "NOT FOUND" })),
            )
            .collect();

        HttpOk::with_data_and_status(
            StatusCode::MULTI_STATUS,
            format!(
                "updated {updated_count}, {} not found in '{db_name}'",
                not_found.len()
            ),
            json!({
                "database": db_name,
                "results": results,
            }),
            &ctx,
        )
        .into_response()
    }
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

//TODO: to batch message uztaisit smuku + lai strada
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

// policy is always TOML — sending raw since it shouldnt be encoded in json/xml
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
