//INFO: under scripts we have ./movies now for funzies

//TODO: update/delete partial replace
//graceful shutdown db.shutdown
//backup/restore
//Some Errors.rs file/enum for standardised errors
//HTTPS auth clustering lmao
//better output
//better LOGS no just prints, or tracing atleast
//and all //TODO ive written

use axum::{
    Router,
    extract::{Path, State},
    middleware,
    routing::{delete, get, post},
};

use core_core::CorelamoDatabase;

use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    process,
    sync::{Arc, RwLock},
};

use tokio::signal;

#[cfg(test)]
mod api_tests;

mod corelamo_settings;
mod database_helpers;
mod doctypes;
mod handlers;
mod response;

#[derive(Clone)]
pub struct AppState {
    pub databases: Arc<RwLock<HashMap<String, CorelamoDatabase>>>,
    pub databases_dir: PathBuf,
    pub default_filetype: String,
}

//helper function for axum to decet the shutdown of a programm
//TODO: maybe check if there are more possible signals
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    #[cfg(unix)]
    let hangup = async {
        signal::unix::signal(signal::unix::SignalKind::hangup())
            .expect("failed to install SIGHUP handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let hangup = std::future::pending::<()>();

    #[cfg(unix)]
    let quit = async {
        signal::unix::signal(signal::unix::SignalKind::quit())
            .expect("failed to install SIGQUIT handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let quit = std::future::pending::<()>();

    let reason = tokio::select! {
        _ = ctrl_c => "SIGINT (Ctrl+C)",
        _ = terminate => "SIGTERM",
        _ = hangup => "SIGHUP",
        _ = quit => "SIGQUIT",
    };

    println!("shutdown signal received: {reason}, shutting down gracefully...");
}
#[tokio::main]
async fn main() -> io::Result<()> {
    let cli_overrides = match corelamo_settings::parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!("Run with --help for usage.");
            process::exit(1);
        }
    };

    println!("corelamo-runtime starting");

    let settings = match corelamo_settings::load_or_init_settings(cli_overrides) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error loading settings: {e}");
            process::exit(1);
        }
    };

    let root_path = PathBuf::from(corelamo_settings::get(&settings, "root-path"));
    let name = corelamo_settings::get(&settings, "name");
    let host = corelamo_settings::get(&settings, "host");
    let port = corelamo_settings::get(&settings, "port");
    let default_filetype = corelamo_settings::get(&settings, "filetype");

    println!("root: {}", root_path.display());
    println!("name: {name}");
    println!("host: {host}");
    println!("port: {port}");
    println!("default filetype: {default_filetype}");

    let databases_dir = root_path.join("databases");
    println!("databases_dir: {}", databases_dir.display());

    let databases = match database_helpers::load_saved_databases(&databases_dir) {
        Ok(dbs) => dbs,
        Err(e) => {
            eprintln!("error loading databases: {e}");
            process::exit(1);
        }
    };

    println!("found and opened {} database(s)", databases.len());

    let state = AppState {
        databases: Arc::new(RwLock::new(databases)),
        databases_dir,
        default_filetype,
    };

    //this clone is ok since it just += 1 for Arc
    let state_for_shutdown = state.clone();

    //INFO: the paths that have /{filetype} have another path without it for default filetype,
    //handlers handle it :)
    let app = Router::new()
        .route(
            "/api/databases/{db_name}/search/{filetype}",
            post(handlers::search_handler),
        )
        .route(
            "/api/databases/{db_name}/search",
            post(handlers::search_handler),
        )
        .route(
            "/api/databases/{db_name}/insert/{filetype}",
            post(handlers::insert_handler),
        )
        .route(
            "/api/databases/{db_name}/insert",
            post(handlers::insert_handler),
        )
        .route(
            "/api/databases/{db_name}/retrieve/{filetype}",
            post(handlers::retrieve_handler),
        )
        .route(
            "/api/databases/{db_name}/retrieve",
            post(handlers::retrieve_handler),
        )
        .route(
            "/api/databases/{db_name}/create-database",
            post(handlers::create_handler),
        )
        .route(
            "/api/databases/{db_name}/delete-database",
            delete(handlers::delete_handler),
        )
        .route("/api/databases", get(handlers::list_databases_handler))
        .route(
            "/api/databases/{db_name}/status",
            get(handlers::stats_handler),
        )
        .route(
            "/api/databases/{db_name}/reindex",
            post(handlers::reindex_handler),
        )
        .route(
            "/api/databases/{db_name}/policy/{filetype}",
            get(handlers::get_policy_handler),
        )
        .route(
            "/api/databases/{db_name}/policy/{filetype}",
            post(handlers::set_policy_handler),
        )
        .route(
            "/api/databases/{db_name}/policy",
            get(handlers::get_policy_handler),
        )
        .route(
            "/api/databases/{db_name}/policy",
            post(handlers::set_policy_handler),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            handlers::auth_middleware,
        ))
        .with_state(state);

    let addr = format!("{host}:{port}");
    println!("starting http server on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("listening on {addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    //something or someone killed our beloved programm
    println!("server stopped, shutting down databases...");

    //some dark magic to gain ownership of databases for shutdown
    let databases = std::mem::take(&mut *state_for_shutdown.databases.write().unwrap());
    for (name, db) in databases {
        println!("shutting down database '{name}'...");
        if let Err(e) = db.shutdown() {
            eprintln!("error shutting down '{name}': {e}");
        }
    }

    //TODO: we might have extra stuff to do here later, for now i cant think of anything else

    println!("Goodbye!");
    Ok(())
}
