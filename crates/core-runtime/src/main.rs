//HEELO WORLD
use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware::from_fn_with_state,
    routing::{delete, get, post},
};

use core_auth::{AuthService, UserDatabase};
use core_core::shard_manager::ShardManager;
use core_index::{
    analyzer::analyzer::Analyzer,
    lsm::{LsmIndex, config::IndexRuntimeConfig},
};

use core_logs::logger;
use core_protocol::{errors::CorelamoError, format::Format};
use core_storage::binary_store::BinaryDocumentStore;
use slog::{debug, error, info, warn};
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
    pub databases: Arc<RwLock<HashMap<String, Arc<ShardManager>>>>,
    pub databases_dir: PathBuf,
    pub default_format: Format,
    pub auth: Arc<RwLock<AuthService>>,
}

impl AppState {
    pub fn lookup(&self, db_name: &str) -> Result<Arc<ShardManager>, CorelamoError> {
        let dbs = self
            .databases
            .read()
            .map_err(|_| CorelamoError::Internal("databases lock poisoned".into()))?;
        dbs.get(db_name)
            .cloned()
            .ok_or_else(|| CorelamoError::NotFound(format!("database '{db_name}' not found")))
    }
}

//TODO: maybe check if there are more possible signals
async fn shutdown_signal() {
    let log = slog_scope::logger();
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

    warn!(log,"Shutting down gracefully..."; "shutdown_signal"=>%reason,);
}

#[tokio::main]
async fn main() -> io::Result<()> {
    //logging izmantojot slog lib
    let (log, _guard) = logger::program_logger();
    let _slog_guard = slog_scope::set_global_logger(log.clone());
    info!(log, "Program started");
    let cli_overrides = match corelamo_settings::parse_args() {
        Ok(overrides) => overrides,
        Err(e) => {
            error!(log, "error parsing cli arguments:"; "error"=> %e);
            process::exit(1);
        }
    };
    let settings = match corelamo_settings::load_or_init_settings(cli_overrides) {
        Ok(s) => s,
        Err(e) => {
            error!(log,"error loading settings:";"error"=> %e);
            process::exit(1);
        }
    };

    let root_path = PathBuf::from(corelamo_settings::get(&settings, "root-path"));

    let name = corelamo_settings::get(&settings, "name");
    let host = corelamo_settings::get(&settings, "host");
    let port = corelamo_settings::get(&settings, "port");
    let default_format_str = corelamo_settings::get(&settings, "format");
    let enable_auth = corelamo_settings::get(&settings, "auth") != "false";
    info!(log, "auth setting resolved";"info" => %enable_auth);
    let default_format = Format::JSON;
    // Format::try_from(default_format_str.as_str()).unwrap_or_else(|e| {
    //     eprintln!("error: invalid 'format' in config/cli: {e}");
    //     process::exit(1);
    // });

    info!(log,
        "Server configuration";
        "root"=>%&root_path.display(),
        "name" =>%name,
        "host" =>%host,
        "port"=>%port,
        "default_format" =>%default_format_str
    );

    let databases_dir = root_path.join("databases");
    info!( log, "databases_dir:";"databases_dir" => %databases_dir.display());

    let databases = match database_helpers::load_saved_shard_managers(&databases_dir) {
        Ok(dbs) => dbs,
        Err(e) => {
            error!(log,"error loading databases " ;"error"=>%e );
            process::exit(1);
        }
    };

    info!(log,"found and loaded database(s) in total"; "databases"=>%databases.len());

    let mut handles: HashMap<String, Arc<ShardManager>> = HashMap::new();

    for (db_name, manager) in databases {
        handles.insert(db_name, Arc::new(manager));
    }

    let users_dir = root_path.join("users");
    std::fs::create_dir_all(&users_dir).expect("failed to create users data dir");

    let store_path = users_dir.join("documents.bin");
    let store = BinaryDocumentStore::open(&store_path).expect("failed to open users store");
    let index_cfg = IndexRuntimeConfig::default();
    let index = LsmIndex::new(index_cfg.flush_threshold); // match the movies construction exactly
    let analyzer = Analyzer::default(); // or however movies builds it

    let user_db = UserDatabase::new(store, index, analyzer);
    let auth = Arc::new(RwLock::new(AuthService::bootstrap(user_db)));

    let state = AppState {
        databases: Arc::new(RwLock::new(handles)),
        databases_dir,
        default_format,
        auth,
    };

    //this clone is ok since it just += 1 for Arc
    let state_for_shutdown = state.clone();

    //login
    //let public_routes = Router::new().route("/api/login", post(handlers::login_handler));
    //pec login
    //god forbid someone breaks this
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
            "/api/databases/{db_name}/lookup",
            post(handlers::lookup_handler),
        )
        .route(
            "/api/databases/{db_name}/retrieve",
            post(handlers::retrieve_handler),
        )
        .route(
            "/api/databases/{db_name}/replace",
            post(handlers::replace_document_handler),
        )
        .route(
            "/api/databases/{db_name}/upsert",
            post(handlers::upsert_document_handler),
        )
        .route(
            "/api/databases/{db_name}/delete",
            delete(handlers::delete_document_handler),
        )
        .route(
            "/api/databases/{db_name}/get-logs",
            get(handlers::get_logs_handler),
        )
        .route(
            "/api/databases/{db_name}/clear-logs",
            delete(handlers::clear_logs_handler),
        )
        .route(
            "/api/databases/{db_name}/create-database",
            post(handlers::create_database_handler),
        )
        .route(
            "/api/databases/{db_name}/clear-database",
            delete(handlers::clear_database_handler),
        )
        .route(
            "/api/databases/{db_name}/delete-database",
            delete(handlers::delete_database_handler),
        )
        .route(
            "/api/databases/{db_name}/start-database",
            post(handlers::start_database_handler),
        )
        .route(
            "/api/databases/{db_name}/stop-database",
            post(handlers::stop_database_handler),
        )
        .route("/api/list-databases", get(handlers::list_databases_handler))
        .route(
            "/api/databases/{db_name}/status",
            get(handlers::stats_handler),
        )
        .route(
            "/api/databases/{db_name}/reindex",
            post(handlers::reindex_handler),
        )
        .route(
            "/api/databases/{db_name}/get-policy",
            get(handlers::get_policy_handler),
        )
        .route(
            "/api/databases/{db_name}/set-policy",
            post(handlers::set_policy_handler),
        )
        .route("/api/users", post(handlers::create_user_handler))
        .route(
            "/api/users/{username}",
            delete(handlers::delete_user_handler),
        )
        .route(
            "/api/users/{username}/password",
            post(handlers::update_user_password_handler),
        )
        .route(
            "/api/users/{username}/roles",
            post(handlers::update_user_roles_handler),
        )
        .route(
            "/api/databases/{db_name}/get-config",
            get(handlers::get_config_handler),
        )
        .route(
            "/api/databases/{db_name}/set-config",
            post(handlers::set_config_handler),
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
        warn!(log, "AUTH DISABLED — You're on your own!");

        protected_routes.layer(axum::middleware::from_fn(
            middleware::disabled_auth_middleware,
        ))
    };

    let app = Router::new()
        //.merge(public_routes)
        .merge(protected_routes)
        //TODO: Make configurable
        .layer(DefaultBodyLimit::max(512 * 1024 * 1024)) // 512 MB
        .layer(from_fn_with_state(
            state.clone(),
            middleware::request_context_middleware,
        ))
        .with_state(state);

    let addr = format!("{host}:{port}");
    debug!(log,"starting http server on";"address"=>%addr);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    debug!(log,"listening on";"address"=>%addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    //something or someone killed our beloved programm
    info!(log, "Server stopped, shutting down databases..");
    let handles = {
        let mut guard = state_for_shutdown.databases.write().unwrap();
        std::mem::take(&mut *guard)
    };
    for (db_name, manager) in handles {
        info!(log,"shutting down database ...";"database"=>%db_name);
        match Arc::try_unwrap(manager) {
            Ok(mgr) => {
                if let Err(e) = mgr.shutdown() {
                    error!(log,"error shutting down database";"db" => %db_name, "error" => %e,);
                }
            }
            Err(_) => {
                error!(
                    log,
                    "could not get exclusive access to database during shutdown, \
                     shard threads may not be joined cleanly";
                    "db" => %db_name,
                );
            }
        }
    }

    //TODO: we might have extra stuff to do here later, for now i cant think of anything else
    info!(log, "Goodbye");
    Ok(())
}
