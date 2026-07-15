//INFO: under scripts we have ./movies now for funzies

//partial replace
//backup/restore
//LOGS not just prints
//reindex should return ok when started not wait the whole time

use axum::{
    Router,
    middleware::from_fn_with_state,
    routing::{delete, get, post, put},
};

use core_auth::AuthService;
use core_core::CorelamoDatabase;
use core_protocol::format::Format;

use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    process,
    sync::{Arc, RwLock},
};

use tokio::signal;

mod corelamo_settings;
mod database_helpers;
mod doctypes;
mod handlers;
mod http_response;
mod middleware;

#[derive(Clone)]
pub struct AppState {
    pub databases: Arc<RwLock<HashMap<String, CorelamoDatabase>>>,
    pub databases_dir: PathBuf,
    pub default_format: Format,
    pub auth: Arc<AuthService>,
}

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

    println!("corelamo-runtime starting...");

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
    let default_format_str = corelamo_settings::get(&settings, "format");
    let enable_auth = corelamo_settings::get(&settings, "auth") != "false";
    let default_format = Format::JSON;
    // Format::try_from(default_format_str.as_str()).unwrap_or_else(|e| {
    //     eprintln!("error: invalid 'format' in config/cli: {e}");
    //     process::exit(1);
    // });

    println!("root: {}", root_path.display());
    println!("name: {name}");
    println!("host: {host}");
    println!("port: {port}");
    println!("default format: {default_format_str}");

    let databases_dir = root_path.join("databases");
    println!("databases_dir: {}", databases_dir.display());

    let databases = match database_helpers::load_saved_databases(&databases_dir) {
        Ok(dbs) => dbs,
        Err(e) => {
            eprintln!("error loading databases: {e}");
            process::exit(1);
        }
    };

    println!("found and loaded {} database(s)", databases.len());
    //Autorizacijas prikoli
    let auth = Arc::new(core_auth::default_auth_service());

    let state = AppState {
        databases: Arc::new(RwLock::new(databases)),
        databases_dir,
        default_format,
        auth,
    };

    //this clone is ok since it just += 1 for Arc
    let state_for_shutdown = state.clone();
    //login
    let public_routes = Router::new().route("/api/login", post(handlers::login_handler));
    //pec login
    let protected_routes = Router::new()
        .route(
            "/api/databases/{db_name}/search",
            post(handlers::search_handler),
        )
        .route(
            "/api/databases/{db_name}/insert",
            post(handlers::insert_handler),
        )
        .route(
            "/api/databases/{db_name}/retrieve",
            post(handlers::retrieve_handler),
        )
        .route(
            "/api/databases/{db_name}/update",
            put(handlers::update_document_handler),
        )
        .route(
            "/api/databases/{db_name}/delete",
            delete(handlers::delete_document_handler),
        )
        //create start stop delete database
        .route(
            "/api/databases/{db_name}/create-database",
            post(handlers::create_database_handler),
        )
        .route(
            "/api/databases/{db_name}/delete-database",
            delete(handlers::delete_detabase_handler),
        )
        .route(
            "/api/databases/{db_name}/start-database",
            post(handlers::start_database_handler),
        )
        .route(
            "/api/databases/{db_name}/stop-database",
            post(handlers::stop_database_handler),
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
            "/api/databases/{db_name}/policy",
            get(handlers::get_policy_handler),
        )
        .route(
            "/api/databases/{db_name}/policy",
            put(handlers::set_policy_handler),
        )
        .route(
            "/api/databases/{db_name}/config",
            get(handlers::get_config_handler),
        )
        .route(
            "/api/databases/{db_name}/config",
            put(handlers::set_config_handler),
        )
        .route(
            "/api/databases/{db_name}/restart-database",
            post(handlers::restart_database_handler),
        );

    let protected_routes = if enable_auth {
        protected_routes.layer(from_fn_with_state(
            state.clone(),
            middleware::auth_middleware,
        ))
    } else {
        println!("auth DISABLED — all routes are public!");
        protected_routes
    };

    let app = Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        .layer(from_fn_with_state(
            state.clone(),
            middleware::request_context_middleware,
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
        //WARN: is the db.shutdown a safe thing to do yet? meaning like while
        //indexing/merging/compacting, if shutdown is it safe?
        if let Err(e) = db.shutdown() {
            eprintln!("error shutting down '{name}': {e}");
        }
    }

    //TODO: we might have extra stuff to do here later, for now i cant think of anything else

    println!("Goodbye!");
    Ok(())
}
