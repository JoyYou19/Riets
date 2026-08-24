use std::{collections::HashMap, io, path::PathBuf, process};

use slog::{info, warn};
//helper file to manage and easilly add corelamo settings

pub const DEFAULT_SETTINGS: &[(&str, &str)] = &[
    ("root-path", "/var/lib/corelamo"),
    ("name", "corelamo"),
    ("host", "0.0.0.0"),
    ("port", "6006"),
    ("format", "json"),
    ("auth", "true"),
    ("max_payload_size", "512"),
    ("max_request_timeout", "30"),
];

pub const HELP: &str = "\
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

    NOTE: config file takes priority over CLI args if it exists (unless --overwrite_config true)

    -h, --h, -help, --help            Print this help message and exit
";

pub fn default_value(key: &str) -> &'static str {
    DEFAULT_SETTINGS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| *v)
        .unwrap_or("")
}

pub fn valid_keys() -> Vec<&'static str> {
    DEFAULT_SETTINGS.iter().map(|(k, _)| *k).collect()
}

pub fn resolve_root(path: PathBuf) -> PathBuf {
    if path.ends_with("corelamo") {
        path
    } else {
        path.join("corelamo")
    }
}

pub fn parse_args() -> Result<HashMap<String, String>, String> {
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
pub fn load_or_init_settings(
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
        info!(log,"config loaded";"settings_path"=>%settings_path.display() );
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

pub fn get(settings: &HashMap<String, String>, key: &str) -> String {
    settings
        .get(key)
        .cloned()
        .unwrap_or_else(|| default_value(key).to_string())
}

// Maybe useful later
pub fn get_usize(settings: &HashMap<String, String>, key: &str) -> usize {
    get(settings, key)
        .parse()
        .unwrap_or_else(|_| default_value(key).parse().unwrap_or(0))
}

pub fn validate_settings(settings: &HashMap<String, String>) -> Result<(), String> {
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
