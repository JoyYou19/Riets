use axum::{
    Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use core_core::CorelamoDatabase;

use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    process,
    sync::{Arc, RwLock},
};

mod database_helpers;

const DEFAULT_ROOT: &str = "/var/lib/corelamo";
const DEFAULT_NAME: &str = "corelamo";
const DEFAULT_PORT: u16 = 6006;
const DEFAULT_HOST: &str = "0.0.0.0";

const HELP: &str = "\
corelamo-runtime 
USAGE:
    corelamo-runtime [OPTIONS]
OPTIONS:
    --root-path <path>    Root directory for all databases and config
                          [default: /var/lib/corelamo]

    --name <name>         Name of this corelamo instance
                          [default: corelamo]

    --host <host>         Host address to bind the HTTP server to
                          [default: 0.0.0.0]
                          note: config file takes priority if it exists

    --port <port>         Port to bind the HTTP server to
                          [default: 6006]
                          note: config file takes priority if it exists

    -h, --help            Print this help message and exit
";

struct Args {
    root: PathBuf,
    name: String,
    host: Option<String>,
    port: Option<u16>,
}

#[derive(Clone)]
pub struct AppState {
    pub databases: Arc<RwLock<HashMap<String, CorelamoDatabase>>>,
    pub databases_dir: PathBuf,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct CorelamoSettings {
    pub name: String,
    pub host: String,
    pub port: u16,
}

impl CorelamoSettings {
    pub fn new(name: String, host: String, port: u16) -> Self {
        Self { name, host, port }
    }

    pub fn databases_dir(&self, root: &PathBuf) -> PathBuf {
        root.join("databases")
    }
}

pub fn load_or_init_settings(
    root: &PathBuf,
    name: String,
    host: Option<String>,
    port: Option<u16>,
) -> io::Result<CorelamoSettings> {
    let settings_path = root.join("DatabaseSettings.toml");

    if settings_path.exists() {
        let raw = std::fs::read_to_string(&settings_path)?;
        let settings: CorelamoSettings =
            toml::from_str(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        println!("config loaded from {}", settings_path.display());
        return Ok(settings);
    }

    println!("no config found, writing defaults...");
    std::fs::create_dir_all(root)?;
    let settings = CorelamoSettings::new(
        name,
        host.unwrap_or_else(|| DEFAULT_HOST.to_string()),
        port.unwrap_or(DEFAULT_PORT),
    );
    let raw =
        toml::to_string_pretty(&settings).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    std::fs::write(&settings_path, raw)?;
    println!("config written to {}", settings_path.display());

    Ok(settings)
}

fn resolve_root(path: PathBuf) -> PathBuf {
    if path.ends_with("corelamo") {
        path
    } else {
        path.join("corelamo")
    }
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1).peekable();
    let mut root: Option<PathBuf> = None;
    let mut name: Option<String> = None;
    let mut host: Option<String> = None;
    let mut port: Option<u16> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{}", HELP);
                process::exit(0);
            }
            "--root-path" => {
                let val = args
                    .next()
                    .ok_or_else(|| "--root-path requires a value".to_string())?;
                root = Some(PathBuf::from(val));
            }
            "--name" => {
                let val = args
                    .next()
                    .ok_or_else(|| "--name requires a value".to_string())?;
                name = Some(val);
            }
            "--host" => {
                let val = args
                    .next()
                    .ok_or_else(|| "--host requires a value".to_string())?;
                host = Some(val);
            }
            "--port" => {
                let val = args
                    .next()
                    .ok_or_else(|| "--port requires a value".to_string())?;
                let parsed = val
                    .parse::<u16>()
                    .map_err(|_| format!("--port must be a valid port number, got: {val}"))?;
                port = Some(parsed);
            }
            other => {
                return Err(format!("unknown argument: {other}"));
            }
        }
    }

    Ok(Args {
        root: resolve_root(root.unwrap_or_else(|| PathBuf::from(DEFAULT_ROOT))),
        name: name.unwrap_or_else(|| DEFAULT_NAME.to_string()),
        host,
        port,
    })
}

async fn search_handler(
    Path(db_name): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> (StatusCode, String) {
    let q = match params.get("q") {
        Some(q) => q.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "missing ?q= query param".to_string(),
            );
        }
    };

    println!("[search] db={db_name} q={q:?}");

    (StatusCode::OK, format!("db={db_name} q={q}"))
}

async fn insert_handler(Path(db_name): Path<String>, body: String) -> (StatusCode, String) {
    println!("[insert] db={db_name} body={body:?}");
    (
        StatusCode::OK,
        format!("db={db_name} received {} bytes", body.len()),
    )
}

async fn create_handler(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
) -> (StatusCode, String) {
    let db_path = state.databases_dir.join(&db_name);

    match database_helpers::create_database(db_name.clone(), &db_path) {
        Ok(db) => {
            state.databases.write().unwrap().insert(db_name.clone(), db);
            (StatusCode::OK, format!("database {db_name} created"))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to create database {db_name}: {e}"),
        ),
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!("Run with --help for usage.");
            process::exit(1);
        }
    };

    println!("corelamo-runtime starting");
    println!("root: {}", args.root.display());

    let settings = match load_or_init_settings(&args.root, args.name, args.host, args.port) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error loading settings: {e}");
            process::exit(1);
        }
    };

    println!("name: {}", settings.name);
    println!("host: {}", settings.host);
    println!("port: {}", settings.port);

    let databases_dir = settings.databases_dir(&args.root);
    println!("databases_dir: {}", databases_dir.display());

    let databases = match database_helpers::load_databases(&databases_dir) {
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
    };

    let addr = format!("{}:{}", settings.host, settings.port);
    println!("starting http server on {addr}");
    let app = Router::new()
        .route("/api/databases/{db_name}/search", get(search_handler))
        .route("/api/databases/{db_name}/insert", post(insert_handler))
        .route("/api/databases/{db_name}/create", post(create_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("listening on {addr}");

    axum::serve(listener, app).await?;

    Ok(())
}
