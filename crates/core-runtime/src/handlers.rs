use std::{
    collections::HashMap,
    sync::{RwLockReadGuard, RwLockWriteGuard},
};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension,
};
use core_core::CorelamoDatabase;
use core_protocol::errors::CorelamoError;
use serde_json::json;

use crate::{
    database_helpers, doctypes,
    middleware::RequestContext,
    response::{HttpError, HttpOk},
    AppState,
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

pub async fn search_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Extension(ctx): Extension<RequestContext>,
    body: String,
) -> Response {
    //TODO smarter shit for query syntax
    let q = match require_body(&body) {
        Ok(q) => q.to_string(),
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    let mut databases = state.databases.write().unwrap();
    let db = match get_db_write(&mut databases, &db_name) {
        Ok(db) => db,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    //TODO docs offset nevis hardcoded 10 + structured query like elastic/CP not raw string
    //TODO process query validity.... all the * AND OR check ......
    //FIX: "cars" search gets over stemmed "car" works
    let hits = match db.search(&q, 10) {
        Ok(hits) => hits,
        //TODO: fix when search returns CorelamoError
        Err(e) => {
            return HttpError::from_corelamo(
                CorelamoError::Internal(format!("search failed: {e}")),
                &ctx,
            )
            .into_response();
        }
    };

    let hit_count = hits.len();
    let output = match doctypes::serialize_hits(hits, ctx.format) {
        Ok(data) => data,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };
    HttpOk::with_data(format!("{hit_count} hit(s) for '{q}'"), output, &ctx).into_response()
}

//TODO: how would we return the exact document that was stored (we deserialize it from HashMap)
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

    let ids: Vec<String> = match serde_json::from_str(&body) {
        Ok(ids) => ids,
        Err(_) => {
            return HttpError::from_corelamo(
                CorelamoError::InvalidData("expected JSON array of ids".to_string()),
                &ctx,
            )
            .into_response();
        }
    };

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

    //FIX: doctypes::convert_from_storage should return typed data directly instead of String
    //     not_found_ids is also dropped here until HttpOk.data can carry structured data
    let output = match doctypes::convert_from_storage(&docs, ctx.format) {
        Ok(data) => data,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };
    HttpOk::with_data(
        format!("retrieved {} document(s)", docs.len()),
        serde_json::json!({ "documents": output, "not_found": not_found_ids }),
        &ctx,
    )
    .into_response()
}

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

    let input_docs = match doctypes::parse_documents(&body, ctx.format) {
        Ok(d) => d,
        Err(e) => return HttpError::from_corelamo(e, &ctx).into_response(),
    };

    if input_docs.is_empty() {
        return HttpError::from_corelamo(
            CorelamoError::InvalidData("no valid documents found in request body".to_string()),
            &ctx,
        )
        .into_response();
    }

    //TODO: needs duplicate check!!!!
    let doc_count = input_docs.len();
    match db.put_documents_parallel(input_docs) {
        Ok(_) => HttpOk::with_data(
            format!("inserted {doc_count} document(s) into '{db_name}'"),
            json!({ "inserted": doc_count, "database": db_name }),
            &ctx,
        )
        .into_response(),
        //TODO: update ones put_documents_parallel gets errors updated
        Err(e) => {
            HttpError::from_corelamo(CorelamoError::Internal(format!("insert failed: {e}")), &ctx)
                .into_response()
        }
    }
}

pub async fn create_handler(
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
pub async fn delete_handler(
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
