use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::StatusCode,
    middleware::from_fn_with_state,
    routing::{delete, get, post},
};
use core_core::shard_manager::ShardManager;
use tower_http::{cors::CorsLayer, decompression::RequestDecompressionLayer};
use tower_http::{timeout::TimeoutLayer, trace::TraceLayer};

use core_auth::{AuthService, MIN_PASSWORD_LEN, SUPERUSER, UserDatabase};
use core_index::{
    analyzer::analyzer::Analyzer,
    lsm::{LsmIndex, config::IndexRuntimeConfig},
};

use core_logs::logger;
use core_protocol::{errors::CorelamoError, format::Format};
use core_storage::binary_store::BinaryDocumentStore;
use slog::{debug, error, info, warn};
use std::{
    fs::{File, OpenOptions, TryLockError},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
    process,
    sync::{Arc, RwLock},
    time::Duration,
};

use tokio::{
    signal,
    sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, watch},
};

use crate::{
    database_helpers::{normalize_db_name, sweep_trash},
    database_manager::{Databases, DbSlot},
};

mod corelamo_settings;
mod database_helpers;
mod database_manager;
mod doc_parser;
mod handlers;
mod http_response;
mod middleware;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// How long open HTTP connections get to finish after a shutdown signal before they are cut.
const HTTP_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the databases get to shut down. Writes are in the WAL before they are applied,
/// so giving up here loses no acknowledged write; it only means more WAL replay at start.
const DB_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the runtime waits for leftover blocking tasks (embedding, backups) at exit.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Exit code for a forced exit on a second shutdown signal (128 + SIGINT).
const FORCED_EXIT_CODE: i32 = 130;

/// Takes an exclusive OS lock on `<root>/corelamo.lock` for the life of the process, so a
/// second server on the same data cannot start
fn lock_root(root: &Path) -> io::Result<File> {
    std::fs::create_dir_all(root)?;
    let path = root.join("corelamo.lock");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            let mut holder = String::new();
            // the pid is only for the message; a failed read must not hide the real error
            let _ = file.read_to_string(&mut holder);
            let holder = holder.trim();
            let holder = if holder.is_empty() { "unknown" } else { holder };
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "{} is locked by another corelamo process (pid {holder}); stop it first",
                    path.display()
                ),
            ));
        }
        Err(TryLockError::Error(e)) => {
            return Err(io::Error::new(
                e.kind(),
                format!("locking {}: {e}", path.display()),
            ));
        }
    }
    file.set_len(0)?;
    file.rewind()?;
    write!(file, "{}", process::id())?;
    file.sync_all()?;
    Ok(file)
}
fn prompt_new_password() -> io::Result<String> {
    use std::io::IsTerminal;
    if !io::stdin().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--reset-password interactive needs a terminal; use random",
        ));
    }
    loop {
        let first = rpassword::prompt_password(format!("New password for '{SUPERUSER}': "))?;
        if first.chars().count() < MIN_PASSWORD_LEN {
            eprintln!("Too short: use at least {MIN_PASSWORD_LEN} characters.");
            continue;
        }
        let second = rpassword::prompt_password("Repeat it: ")?;
        if first == second {
            return Ok(first);
        }
        eprintln!("The passwords do not match, try again.");
    }
}
fn ask_to_set_password() -> io::Result<Option<String>> {
    use std::io::IsTerminal;
    if !io::stdin().is_terminal() {
        return Ok(None);
    }
    eprint!("Set your own password now? [y/N]: ");
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        return Ok(None);
    }
    prompt_new_password().map(Some)
}

fn print_password(password: &str) {
    eprintln!(
        "\n  user:     {SUPERUSER}\n  password: {password}\n  Shown once, keep it safe, the password cannot be recovered\n"
    );
}
#[derive(Clone)]
pub struct AppState {
    pub databases: Arc<Databases>,
    pub default_format: Format,
    pub default_pretty: bool,
    pub auth: Arc<RwLock<AuthService>>,
}

impl AppState {
    //all others
    pub async fn db_read(
        &self,
        db_name: &str,
    ) -> Result<(Arc<ShardManager>, OwnedRwLockReadGuard<()>), CorelamoError> {
        let slot = self.databases.get(db_name)?;
        let gate = slot.read().await;
        Ok((slot.manager()?, gate))
    }

    //for if/how documents are inserted start, stop, restart, clear, reindex, abort-reindex, set-policy, set-config, set-dictionary, reset-dictionary, restore
    pub async fn db_write(
        &self,
        db_name: &str,
    ) -> Result<(Arc<ShardManager>, OwnedRwLockWriteGuard<()>), CorelamoError> {
        let slot = self.databases.get(db_name)?;
        let gate = slot.write().await;
        Ok((slot.manager()?, gate))
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

fn main() -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run());
    // dropping the runtime would wait without limit for every spawn_blocking task still
    // running (model load, embedding, backup), which kept the process alive after shutdown
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    result
}

async fn run() -> io::Result<()> {
    //logging izmantojot slog lib
    let cli_overrides = match corelamo_settings::parse_args() {
        Ok(overrides) => overrides,
        Err(_) => {
            process::exit(1);
        }
    };
    let settings = match corelamo_settings::load_or_init_settings(cli_overrides) {
        Ok(s) => s,
        Err(_) => {
            process::exit(1);
        }
    };

    if let Err(_) = corelamo_settings::validate_settings(&settings) {
        process::exit(1);
    }
    let root_path = PathBuf::from(corelamo_settings::get(&settings, "root-path"));
    let (log, _guard) = logger::program_logger(&root_path);
    let _slog_guard = slog_scope::set_global_logger(log.clone());
    info!(log, "Program started");

    // held until the process exits; must be taken before any database file is touched
    let _root_lock = match lock_root(&root_path) {
        Ok(file) => file,
        Err(e) => {
            error!(log, "cannot start: data directory is in use"; "error" => %e);
            eprintln!("error: {e}");
            process::exit(1);
        }
    };

    let name = corelamo_settings::get(&settings, "name");
    let host = corelamo_settings::get(&settings, "host");
    let port = corelamo_settings::get(&settings, "port");
    let max_payload_size = corelamo_settings::get_usize(&settings, "max_payload_size");
    let max_request_timeout = corelamo_settings::get_usize(&settings, "max_request_timeout");
    let default_format_str = corelamo_settings::get(&settings, "format");
    let default_pretty = corelamo_settings::get_bool(&settings, "pretty");
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
    info!(log, "databases_dir:";"databases_dir" => %databases_dir.display());

    if let Err(e) = corelamo_settings::init_synonyms(&settings) {
        error!(log, "error loading the synonym dictionary"; "error" => %e);
        process::exit(1);
    }
    if let Err(e) = corelamo_settings::init_models(&settings) {
        error!(log, "error setting up embedding models"; "error" => %e);
        process::exit(1);
    }

    //clear all directories that somehow didnt get deleted before restart
    sweep_trash(&databases_dir);

    let databases = match database_helpers::load_saved_shard_managers(&databases_dir) {
        Ok(dbs) => dbs,
        Err(e) => {
            error!(log,"error loading databases " ;"error"=>%e);
            process::exit(1);
        }
    };

    info!(log,"found and loaded database(s) in total"; "databases"=>%databases.len());

    let databases_registry = Databases::new(databases_dir.clone());
    for (db_name, manager) in databases {
        let db_name = normalize_db_name(&db_name);
        let dir = manager.root().to_path_buf();
        let manager = Arc::new(manager);
        manager.start_backup_scheduler();
        let slot = DbSlot::live(db_name.clone(), dir, manager);
        if let Err(e) = databases_registry.insert(Arc::new(slot)) {
            error!(log, "database not registered at boot"; "db" => %db_name, "error" => %e);
        }
    }

    let users_dir = root_path.join("users");
    std::fs::create_dir_all(&users_dir).expect("failed to create users data dir");
    let store_root = users_dir.join("documents");
    let store = BinaryDocumentStore::open(&store_root).expect("failed to open users store");
    let index_cfg = IndexRuntimeConfig::default();
    let index = LsmIndex::new(index_cfg.flush_threshold); // match the movies construction exactly
    let analyzer = Analyzer::default(); // or however movies builds it

    let user_db = UserDatabase::new(store, index, analyzer);
    let auth = match AuthService::bootstrap(user_db) {
        Ok((mut auth, created)) => {
            if let Some(password) = created {
                eprintln!("\n Inital start: superuser created.");
                print_password(&password);
                warn!(log, "created the superuser with a generated password (printed on stderr)";
                    "user" => SUPERUSER);
                // first start only: offer to replace it while someone is at the terminal
                match tokio::task::spawn_blocking(ask_to_set_password).await {
                    Ok(Ok(Some(own))) => match auth.set_initial_password(&own) {
                        Ok(()) => {
                            eprintln!("Password for '{SUPERUSER}' updated.");
                            info!(log, "superuser password set at first start"; "user" => SUPERUSER);
                        }
                        Err(e) => eprintln!("error: {e}; the printed password stays valid"),
                    },
                    Ok(Ok(None)) => {}
                    Ok(Err(e)) => eprintln!("error: {e}; the printed password stays valid"),
                    Err(e) => eprintln!(
                        "error: password prompt failed: {e}; the printed password stays valid"
                    ),
                }
            }
            Arc::new(RwLock::new(auth))
        }
        Err(e) => {
            error!(log, "cannot start: user bootstrap failed"; "error" => %e);
            eprintln!("error: {e}");
            process::exit(1);
        }
    };

    let state = AppState {
        databases: Arc::new(databases_registry),
        default_pretty,
        default_format,
        auth,
    };

    //this clone is ok since it just += 1 for Arc
    let state_for_shutdown = state.clone();

    //login
    let public_routes = Router::new().route("/api/login", post(handlers::login_handler));
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
            "/api/databases/{db_name}/partial-replace",
            post(handlers::partial_replace_handler),
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
            "/api/databases/{db_name}/did-you-mean",
            post(handlers::did_you_mean_handler),
        )
        .route(
            "/api/databases/{db_name}/get-logs",
            get(handlers::get_logs_handler),
        )
        .route(
            "/api/databases/{db_name}/clear-logs",
            delete(handlers::clear_logs_handler),
        )
        // .route("/api/databases/[db_name}/cleanup", post(handlers::cleanup_handler))
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
            "/api/databases/{db_name}/rename-database",
            post(handlers::rename_database_handler),
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
        .route("/api/list-models", get(handlers::list_models_handler))
        .route(
            "/api/databases/{db_name}/status",
            get(handlers::stats_handler),
        )
        .route(
            "/api/databases/{db_name}/reindex",
            post(handlers::reindex_handler),
        )
        .route(
            "/api/databases/{db_name}/reindex/abort",
            post(handlers::abort_reindex_handler),
        )
        .route(
            "/api/databases/{db_name}/get-policy",
            get(handlers::get_policy_handler),
        )
        .route(
            "/api/databases/{db_name}/set-policy",
            post(handlers::set_policy_handler),
        )
        .route(
            "/api/databases/{db_name}/info-words",
            post(handlers::info_words_handler),
        )
        .route("/api/users", post(handlers::create_user_handler))
        .route(
            "/api/users/{username}",
            delete(handlers::delete_user_handler),
        )
        .route("/api/users/list-users", get(handlers::list_users_handler))
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
            "/api/databases/{db_name}/dictionary",
            get(handlers::get_dictionary_handler),
        )
        .route(
            "/api/databases/{db_name}/set-dictionary",
            post(handlers::set_dictionary_handler),
        )
        .route(
            "/api/databases/{db_name}/reload-dictionary",
            post(handlers::reset_dictionary_handler),
        )
        .route(
            "/api/databases/{db_name}/set-config",
            post(handlers::set_config_handler),
        )
        .route(
            "/api/databases/{db_name}/all-fields",
            get(handlers::get_all_fields_handler),
        )
        .route(
            "/api/databases/{db_name}/restart-database",
            post(handlers::restart_database_handler),
        )
        .route(
            "/api/databases/{db_name}/backup",
            post(handlers::backup_handler),
        )
        .route(
            "/api/databases/{db_name}/delete-backup/{backup_id}",
            delete(handlers::backup_delete_handler),
        )
        .route(
            "/api/databases/{db_name}/backup/incremental",
            post(handlers::backup_incremental_handler),
        )
        .route(
            "/api/databases/{db_name}/list-backups",
            get(handlers::list_backups_handler),
        )
        .route(
            "/api/databases/{db_name}/restore-backup/{backup_id}",
            post(handlers::backup_restore_handler),
        )
        .route("/api/timings", post(handlers::timings_handler))
        .route(
            "/api/databases/{db_name}/disk-usage",
            get(handlers::disk_usage_handler),
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

    // let governor_conf = Arc::new(
    //     GovernorConfigBuilder::default()
    //         .per_second(10) // refill rate
    //         .burst_size(100) // max burst
    //         .finish()
    //         .unwrap(),
    // );

    //TODO: HTTTPS + tas CorsLayer vajag uztaisit more secure jo Allow-Origin: * is a big no-no
    let app = Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        .layer(RequestDecompressionLayer::new())
        .layer(DefaultBodyLimit::max(max_payload_size * 1024 * 1024))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            //we parse this as uzise to bypass negative time and shit
            Duration::from_secs(max_request_timeout as u64),
        ))
        //to and from gzip n shit
        // .layer(CompressionLayer::new())
        ////////////////////////////
        .layer(from_fn_with_state(
            state.clone(),
            middleware::request_context_middleware,
        ))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = format!("{host}:{port}");
    debug!(log,"starting http server on";"address"=>%addr);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    debug!(log,"listening on";"address"=>%addr);

    // first signal: graceful shutdown; a second signal: exit at once
    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = stop_tx.send(true);
        shutdown_signal().await;
        let log = slog_scope::logger();
        error!(log, "second shutdown signal, exiting without waiting");
        process::exit(FORCED_EXIT_CODE);
    });

    let mut graceful_rx = stop_rx.clone();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = graceful_rx.wait_for(|stop| *stop).await;
        })
        .into_future();
    tokio::pin!(server);

    // graceful shutdown alone waits for every open connection, so one idle keep-alive client
    // kept the process alive forever; after the drain timeout the remaining ones are cut
    let mut deadline_rx = stop_rx;
    tokio::select! {
        served = &mut server => served?,
        () = async {
            let _ = deadline_rx.wait_for(|stop| *stop).await;
            tokio::time::sleep(HTTP_DRAIN_TIMEOUT).await;
        } => {
            warn!(log, "connections still open after the drain timeout, closing them";
                "timeout_secs" => HTTP_DRAIN_TIMEOUT.as_secs());
        }
    }

    //something or someone killed our beloved programm
    info!(log, "Server stopped, shutting down databases..");

    // ShardManager::shutdown blocks (it waits for shard threads); run it off the runtime
    // threads and stop waiting after a limit, so a stuck shard cannot keep the process alive
    let shutdown_log = log.clone();
    let databases = Arc::clone(&state_for_shutdown.databases);
    let shutdown = tokio::task::spawn_blocking(move || {
        for slot in databases.list() {
            let Ok(manager) = slot.manager() else {
                info!(shutdown_log, "skipping transient database at shutdown"; "db" => %slot.name);
                continue;
            };
            info!(shutdown_log, "shutting down database ..."; "database" => %slot.name);
            if let Err(e) = manager.shutdown() {
                error!(shutdown_log, "error shutting down database";
                "db" => %slot.name, "error" => %e);
            }
        }
    });
    match tokio::time::timeout(DB_SHUTDOWN_TIMEOUT, shutdown).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            error!(log, "database shutdown task failed"; "error" => %e);
        }
        Err(_) => {
            error!(log, "databases did not shut down in time, exiting anyway";
                "timeout_secs" => DB_SHUTDOWN_TIMEOUT.as_secs());
        }
    }

    //TODO: we might have extra stuff to do here later, for now i cant think of anything else
    info!(log, "Goodbye");
    Ok(())
}

