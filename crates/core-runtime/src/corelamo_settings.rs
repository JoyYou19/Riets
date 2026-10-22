use core_embed::{ModelConfig, ModelRegistry};
use core_query::dictionary::SynonymRegistry;
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    process,
    sync::{Arc, OnceLock},
};

use slog::{info, warn};
//helper file to manage and easilly add corelamo settings

pub(crate) const DEFAULT_SETTINGS: &[(&str, &str)] = &[
    ("root-path", "/var/lib/corelamo"),
    ("name", "corelamo"),
    ("host", "0.0.0.0"),
    ("port", "6006"),
    ("format", "json"),
    ("pretty", "true"),
    ("auth", "true"),
    ("max_payload_size", "1024"),
    ("max_request_timeout", "240"),
    ("embedding-model", "arctic-embed-m"),
    ("embedding-device", "auto"),
    ("embedding-runtime", ""),
    ("embedding-instances", "1"),
    ("embedding-batch-size", "64"),
    ("embedding-max-tokens", "256"),
    ("embedding-max-chars", "2000"),
];

pub(crate) const HELP: &str = "\
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
                          

    --port <port>         Port to bind the HTTP server to
                          [default: 6006]

    --format <format>     Default format for requests, responses and documents (json)
                          [default: json]

    --auth <true|false>   Enable/Disabe any form of auth  
                          [default: true]

    --max_payload_size <MB>      
                          Maximum payload size for a single requests  
                          [default: 512MB]

    --max_request_timeout <s>      
                          Maximum time to wait before responding back to the user 
                          [default: 30s]

    --overwrite_config <true|false>
                          If true, CLI args override config file values.
                          If false, config file values override CLI args.
                          [default: false]
                          [CLI only - not persisted to config file]
    --embedding-model <id>
                          Embedding model for vector fields, shared by all databases and config
                          (bge-small-en-v1.5, multilingual-e5-small,
                          snowflake-arctic-embed-s, snowflake-arctic-embed-m)
                          [default: bge-small-en-v1.5]

    NOTE: config file takes priority over CLI args if it exists (unless --overwrite_config true)

    -h, --h, -help, --help            Print this help message and exit

";

pub(crate) fn default_value(key: &str) -> &'static str {
    DEFAULT_SETTINGS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| *v)
        .unwrap_or("")
}

pub(crate) fn valid_keys() -> Vec<&'static str> {
    DEFAULT_SETTINGS.iter().map(|(k, _)| *k).collect()
}

pub(crate) fn resolve_root(path: PathBuf) -> PathBuf {
    if path.ends_with("corelamo") {
        path
    } else {
        path.join("corelamo")
    }
}

pub(crate) fn parse_args() -> Result<HashMap<String, String>, String> {
    let mut overrides = HashMap::new();
    let mut args = std::env::args().skip(1).peekable();
    let keys = valid_keys();
    while let Some(arg) = args.next() {
        if arg == "-h" || arg == "--help" || arg == "--h" || arg == "-help" {
            println!("{}", HELP);
            process::exit(0);
        }

        let key = arg
            .strip_prefix("--")
            .ok_or_else(|| format!("unknown argument: {arg}"))?;

        // Allow overwrite_config as CLI-only option
        if key == "overwrite_config" {
            let val = args
                .next()
                .ok_or_else(|| format!("--{key} requires a value (true/false)"))?;
            if val != "true" && val != "false" {
                return Err(format!(
                    "--overwrite_config must be 'true' or 'false', got '{val}'"
                ));
            }
            overrides.insert(key.to_string(), val);
            continue;
        }

        if !keys.contains(&key) {
            return Err(format!("unknown argument: --{key}"));
        }

        let val = args
            .next()
            .ok_or_else(|| format!("--{key} requires a value"))?;

        overrides.insert(key.to_string(), val);
    }

    Ok(overrides)
}

//config wins over cli, cli wins over hardcoded defaults
//unless overwrite_config is true, then cli wins over config AND gets written to file
pub(crate) fn load_or_init_settings(
    cli_overrides: HashMap<String, String>,
) -> io::Result<HashMap<String, String>> {
    //root path is either default or specified cause we dont know where it is lmao
    let root_path_str = cli_overrides
        .get("root-path")
        .cloned()
        .unwrap_or_else(|| default_value("root-path").to_string());
    let root_path = resolve_root(PathBuf::from(root_path_str));

    let settings_path = root_path.join("CorelamoSettings.toml");
    let log = slog_scope::logger();
    let mut settings: HashMap<String, String> = if settings_path.exists() {
        let raw = std::fs::read_to_string(&settings_path)?;
        info!(log,"config loaded";"settings_path"=>%settings_path.display());
        toml::from_str(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
    } else {
        warn!(log, "No config found,writing defaults..");
        std::fs::create_dir_all(&root_path)?;
        HashMap::new()
    };

    let config_existed = settings_path.exists();

    // Check if user wants to overwrite config with CLI args
    let overwrite_config = cli_overrides
        .get("overwrite_config")
        .map(|s| s == "true")
        .unwrap_or(false);

    if overwrite_config {
        info!(
            log,
            "overwrite_config enabled: CLI args will override config file and be persisted"
        );
    }

    // fill in any missing keys:
    // - if overwrite_config=true: cli > config > defaults (and persist CLI changes)
    // - if overwrite_config=false (default): config > cli > defaults
    for (key, default) in DEFAULT_SETTINGS {
        if overwrite_config {
            // CLI wins over config, and gets persisted
            if let Some(cli_value) = cli_overrides.get(*key) {
                settings.insert(key.to_string(), cli_value.clone());
            } else {
                // No CLI override, keep config or use default
                settings
                    .entry(key.to_string())
                    .or_insert_with(|| default.to_string());
            }
        } else {
            // Config wins over CLI (current behavior)
            settings.entry(key.to_string()).or_insert_with(|| {
                cli_overrides
                    .get(*key)
                    .cloned()
                    .unwrap_or_else(|| default.to_string())
            });
        }
    }

    settings.insert(
        "root-path".to_string(),
        root_path.to_string_lossy().to_string(),
    );

    settings.remove("overwrite_config");

    if !config_existed || overwrite_config {
        let raw = toml::to_string_pretty(&settings).map_err(io::Error::other)?;
        std::fs::write(&settings_path, raw)?;
        if overwrite_config {
            info!(log, "config updated with CLI overrides";"settings_path"=>%settings_path.display());
        } else {
            info!(log, "config written to";"settings_path"=>%settings_path.display());
        }
    }

    Ok(settings)
}

pub(crate) fn get_bool(settings: &HashMap<String, String>, key: &str) -> bool {
    match get(settings, key).to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => true,
        _ => false,
    }
}

pub(crate) fn get(settings: &HashMap<String, String>, key: &str) -> String {
    settings
        .get(key)
        .cloned()
        .unwrap_or_else(|| default_value(key).to_string())
}

// Maybe useful later
pub(crate) fn get_usize(settings: &HashMap<String, String>, key: &str) -> usize {
    get(settings, key)
        .parse()
        .unwrap_or_else(|_| default_value(key).parse().unwrap_or(0))
}

pub(crate) fn validate_settings(settings: &HashMap<String, String>) -> Result<(), String> {
    let max_payload_size: usize = get(settings, "max_payload_size").parse().map_err(|_| {
        format!(
            "invalid max_payload_size '{}': must be a valid integer",
            get(settings, "max_payload_size")
        )
    })?;

    if max_payload_size < 1 {
        return Err(format!(
            "invalid max_payload_size {}: must be at least 1 MB",
            max_payload_size
        ));
    }

    let max_request_timeout: usize =
        get(settings, "max_request_timeout").parse().map_err(|_| {
            format!(
                "invalid max_request_timeout '{}': must be a valid integer",
                get(settings, "max_payload_size")
            )
        })?;

    if max_request_timeout < 1 {
        return Err(format!(
            "invalid max_request_timeout {}: must be at least 1s",
            max_request_timeout
        ));
    }

    Ok(())
}

static SYNONYM_REGISTRY: OnceLock<SynonymRegistry> = OnceLock::new();

/// Opens CorelamoDictionary.toml next to CorelamoSettings.toml (written from the built-in copy on
/// first start); every database copies it when it is created.
pub(crate) fn init_synonyms(settings: &HashMap<String, String>) -> Result<(), String> {
    let root_path = PathBuf::from(get(settings, "root-path"));
    let registry = SynonymRegistry::open(&root_path)
        .map_err(|e| format!("failed to load the synonym dictionary: {e}"))?;
    let log = slog_scope::logger();
    info!(log, "synonym dictionary loaded"; "template_path" => %registry.template_path().display());
    SYNONYM_REGISTRY
        .set(registry)
        .map_err(|_| "synonym registry initialised twice".to_string())
}

pub(crate) fn synonym_registry() -> &'static SynonymRegistry {
    SYNONYM_REGISTRY
        .get()
        .expect("synonym registry used before corelamo_settings::init_synonyms in main")
}

static MODEL_REGISTRY: OnceLock<Arc<ModelRegistry>> = OnceLock::new();

/// Sets up the server-wide model registry. Models live in `<root>/models` and are loaded on
/// first use, so a server whose databases have no vector fields never loads one.
pub(crate) fn init_models(settings: &HashMap<String, String>) -> Result<(), String> {
    let root_path = PathBuf::from(get(settings, "root-path"));
    let runtime_setting = get(settings, "embedding-runtime");
    let install_runtime = runtime_setting.trim().is_empty();
    let runtime_library = if install_runtime {
        core_embed::default_runtime_library(&root_path.join("runtime"))
    } else {
        PathBuf::from(runtime_setting.trim())
    };
    let device = core_embed::DevicePreference::parse(&get(settings, "embedding-device"))
        .map_err(|e| format!("invalid embedding settings: {e}"))?;
    let config = ModelConfig {
        model_id: get(settings, "embedding-model"),
        instances: get_usize(settings, "embedding-instances"),
        batch_size: get_usize(settings, "embedding-batch-size"),
        max_tokens: get_usize(settings, "embedding-max-tokens"),
        max_chars: get_usize(settings, "embedding-max-chars"),
        device,
        runtime_library,
        install_runtime,
    };
    let registry = ModelRegistry::new(root_path.join("models"), config.clone())
        .map_err(|e| format!("invalid embedding settings: {e}"))?;
    let log = slog_scope::logger();
    info!(log, "embedding models configured";
        "config" => ?config,
        "cache_dir" => %registry.cache_dir().display(),
        "runtime_library" => %config.runtime_library.display(),
        "runtime_found" => config.runtime_library.is_file());
    MODEL_REGISTRY
        .set(Arc::new(registry))
        .map_err(|_| "model registry initialised twice".to_string())
}

pub(crate) fn model_registry() -> Arc<ModelRegistry> {
    Arc::clone(
        MODEL_REGISTRY
            .get()
            .expect("model registry used before corelamo_settings::init_models in main"),
    )
}
