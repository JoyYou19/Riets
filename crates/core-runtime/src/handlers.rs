use std::{
    collections::HashMap,
    io,
    sync::{RwLockReadGuard, RwLockWriteGuard},
};

use axum::{
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::Response,
};
use core_core::CorelamoDatabase;

use crate::{AppState, database_helpers, doctypes, response};

fn get_db_read<'a>(
    databases: &'a RwLockReadGuard<HashMap<String, CorelamoDatabase>>,
    db_name: &str,
) -> Result<&'a CorelamoDatabase, response::ApiResponse> {
    databases
        .get(db_name)
        .ok_or_else(|| response::not_found(&format!("database '{db_name}' not found")))
}

fn get_db_write<'a>(
    databases: &'a mut RwLockWriteGuard<HashMap<String, CorelamoDatabase>>,
    db_name: &str,
) -> Result<&'a mut CorelamoDatabase, response::ApiResponse> {
    databases
        .get_mut(db_name)
        .ok_or_else(|| response::not_found(&format!("database '{db_name}' not found")))
}

fn require_body(body: &str) -> Result<&str, response::ApiResponse> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        Err(response::bad_request("request body is empty"))
    } else {
        Ok(trimmed)
    }
}

//resolves db_name (passthrough) + the Format to use for this request.
//Accept header wins if present and parseable; missing/empty/*/* falls back to config default.
//a named-but-unsupported subtype is a hard error (406) rather than a silent fallback.
//TODO: this ignores q= quality values and just takes the first listed type in a
//comma-separated Accept header (e.g. "application/xml;q=0.9, application/json" picks xml).
fn get_db_and_format(
    state: &AppState,
    headers: &HeaderMap,
    db_name: String,
) -> Result<(String, doctypes::Format), response::ApiResponse> {
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());

    let format = match accept {
        None => state.default_format,
        Some(accept) => {
            let first = accept.split(',').next().unwrap_or("").trim();
            let subtype = first.split('/').nth(1).unwrap_or("").trim();

            if first.is_empty() || subtype.is_empty() || subtype == "*" {
                state.default_format
            } else {
                doctypes::Format::try_from(subtype).map_err(|_| {
                    response::error(
                        StatusCode::NOT_ACCEPTABLE,
                        &format!("unsupported format in Accept header: '{subtype}'"),
                    )
                })?
            }
        }
    };

    Ok((db_name, format))
}

//TODO auth/https before request (check permissions....)
pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let token = request.headers().get("X-Corelamo-Key");

    //HACK:  japieliek ka parbauda sis vienkarsi taads placeholder
    next.run(request).await

    // match token {
    //     Some(key) if (key == "mysecretkey") || (true) => next.run(request).await,
    //     _ => (StatusCode::UNAUTHORIZED, "missing or invalid api key").into_response(),
    // }
}

pub async fn search_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    headers: HeaderMap,
    body: String,
) -> response::ApiResponse {
    let q = match require_body(&body) {
        Ok(q) => q.to_string(),
        Err(e) => return e,
    };

    let (db_name, format) = match get_db_and_format(&state, &headers, db_name) {
        Ok(r) => r,
        Err(e) => return e,
    };

    let mut databases = state.databases.write().unwrap();
    let db = match get_db_write(&mut databases, &db_name) {
        Ok(db) => db,
        Err(e) => return e,
    };

    //TODO docs offset nevis hardcoded 10 + structured query like elastic/CP not raw string
    //TODO process query validity.... all the * AND OR  check ......
    //FIX: "cars" search gets over stemmed "car" works
    let hits = match db.search(&q, 10) {
        Ok(hits) => hits,
        Err(e) => return response::internal_error(&format!("search failed: {e}")),
    };

    let hit_count = hits.len();
    let output = match doctypes::serialize_hits(hits, format) {
        Ok(s) => s,
        Err(e) => return response::bad_request(&e.to_string()),
    };

    response::ok_with_data(
        &format!("{hit_count} hit(s) for '{q}'"),
        serde_json::from_str(&output).unwrap(),
    )
}

//TODO how would we return the exact document that was stored (we deserialize it from HashMap)
pub async fn retrieve_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    headers: HeaderMap,
    body: String,
) -> response::ApiResponse {
    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => return e,
    };

    let ids: Vec<String> = match serde_json::from_str(&body) {
        Ok(ids) => ids,
        Err(e) => return response::bad_request(&format!("expected JSON array of ids: {e}")),
    };

    let (db_name, format) = match get_db_and_format(&state, &headers, db_name) {
        Ok(r) => r,
        Err(e) => return e,
    };

    let mut databases = state.databases.write().unwrap();
    let db = match get_db_write(&mut databases, &db_name) {
        Ok(db) => db,
        Err(e) => return e,
    };

    let mut docs = Vec::new();
    let mut not_found_ids = Vec::new();

    for id in &ids {
        match db.get_document(id) {
            Ok(Some(doc)) => docs.push(doc),
            Ok(None) => not_found_ids.push(id.clone()),
            Err(e) => {
                return response::internal_error(&format!("failed to get document '{id}': {e}"));
            }
        }
    }

    let output = match doctypes::convert_from_storage(&docs, format) {
        Ok(s) => s,
        Err(e) => return response::bad_request(&e.to_string()),
    };

    response::ok_with_data(
        &format!("retrieved {} document(s)", docs.len()),
        serde_json::json!({
            "documents": serde_json::from_str::<serde_json::Value>(&output).unwrap(),
            "not_found": not_found_ids,
        }),
    )
}

pub async fn insert_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    headers: HeaderMap,
    body: String,
) -> response::ApiResponse {
    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => return e,
    };

    let (db_name, format) = match get_db_and_format(&state, &headers, db_name) {
        Ok(r) => r,
        Err(e) => return e,
    };

    //db exists?
    let mut databases = state.databases.write().unwrap();
    let db = match get_db_write(&mut databases, &db_name) {
        Ok(db) => db,
        Err(e) => return e,
    };

    //try to parse
    let input_docs = match doctypes::parse_documents(&body, format) {
        Ok(d) => d,
        Err(e) => return response::bad_request(&e.to_string()),
    };

    //insert
    //TODO: needs duplicate check!!!!
    let doc_count = input_docs.len();
    if input_docs.is_empty() {
        return response::bad_request("no valid documents found in request body");
    }

    match db.put_documents_parallel(input_docs) {
        Ok(_) => response::ok_with_data(
            &format!("inserted {doc_count} document(s) into '{db_name}'"),
            serde_json::json!({ "inserted": doc_count, "database": db_name }),
        ),
        Err(e) => response::internal_error(&format!("insert failed: {e}")),
    }
}

pub async fn create_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
) -> response::ApiResponse {
    let databases_read = state.databases.read().unwrap();
    match database_helpers::create_database(db_name.clone(), &state.databases_dir, &databases_read)
    {
        Ok(db) => {
            //realease the read so that we can write
            drop(databases_read);
            state.databases.write().unwrap().insert(db_name.clone(), db);
            response::created(&format!("database '{db_name}' created"))
        }
        Err(e) => {
            //pareizo kluudu izvadam
            let status = match e.kind() {
                io::ErrorKind::AlreadyExists => StatusCode::CONFLICT,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            response::error(
                status,
                &format!("failed to create database '{db_name}': {e}"),
            )
        }
    }
}

pub async fn delete_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
) -> response::ApiResponse {
    let db_path = state.databases_dir.join(&db_name);

    let mut databases_write = state.databases.write().unwrap();

    if !databases_write.contains_key(&db_name) {
        return response::not_found(&format!("database '{db_name}' not found"));
    }

    // shutdown the db before removing it from disk
    if let Some(db) = databases_write.remove(&db_name) {
        if let Err(e) = db.shutdown() {
            return response::internal_error(&format!(
                "failed to shutdown database '{db_name}': {e}"
            ));
        }
    }

    if let Err(e) = std::fs::remove_dir_all(&db_path) {
        return response::internal_error(&format!(
            "removed from memory but failed to delete '{db_name}' from disk: {e}"
        ));
    }

    response::ok(&format!("database '{db_name}' deleted"))
}

pub async fn stats_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
) -> response::ApiResponse {
    let databases = state.databases.read().unwrap();
    let db = match get_db_read(&databases, &db_name) {
        Ok(db) => db,
        Err(e) => return e,
    };

    match db.stats() {
        Ok(stats) => response::ok_with_data(
            &format!("stats for '{db_name}'"),
            serde_json::json!({
                "document_count": stats.document_count,
                "segment_count": stats.segment_count,
                "background_compaction_enabled": stats.background_compaction_enabled,
            }),
        ),
        Err(e) => response::internal_error(&format!("failed to get stats: {e}")),
    }
}

pub async fn reindex_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
) -> response::ApiResponse {
    let mut databases = state.databases.write().unwrap();
    let db = match get_db_write(&mut databases, &db_name) {
        Ok(db) => db,
        Err(e) => return e,
    };

    match db.reindex() {
        Ok(_) => response::ok(&format!("reindex complete for '{db_name}'")),
        Err(e) => response::internal_error(&format!("reindex failed: {e}")),
    }
}

//policy is always TOML now — no format resolution needed, db_name comes straight from the path.
pub async fn get_policy_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
) -> response::ApiResponse {
    let databases = state.databases.read().unwrap();

    let db = match get_db_read(&databases, &db_name) {
        Ok(db) => db,
        Err(e) => return e,
    };

    match doctypes::serialize_policy(db.policy()) {
        Ok(output) => response::ok(&output),
        Err(e) => response::bad_request(&e.to_string()),
    }
}

pub async fn set_policy_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    body: String,
) -> response::ApiResponse {
    let body = match require_body(&body) {
        Ok(b) => b.to_string(),
        Err(e) => return e,
    };

    let policy = match doctypes::parse_policy(&body) {
        Ok(p) => p,
        Err(e) => return response::bad_request(&e.to_string()),
    };

    let mut databases = state.databases.write().unwrap();
    let db = match get_db_write(&mut databases, &db_name) {
        Ok(db) => db,
        Err(e) => return e,
    };

    match db.set_policy(policy) {
        Ok(_) => response::ok(&format!("policy updated for '{db_name}'")),
        Err(e) => match e.kind() {
            io::ErrorKind::InvalidData => response::bad_request(&e.to_string()),
            _ => response::internal_error(&format!("failed to set policy: {e}")),
        },
    }
}

pub async fn list_databases_handler(State(state): State<AppState>) -> response::ApiResponse {
    let databases = state.databases.read().unwrap();
    let names: Vec<&String> = databases.keys().collect();

    response::ok_with_data(
        &format!("{} database(s) loaded", names.len()),
        serde_json::json!({ "databases": names }),
    )
}
