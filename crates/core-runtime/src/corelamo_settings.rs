use std::{collections::HashMap, io, path::PathBuf, process};
//helper file to manage and easilly add corelamo settings

pub const DEFAULT_SETTINGS: &[(&str, &str)] = &[
    ("root-path", "/var/lib/corelamo"),
    ("name", "corelamo"),
    ("host", "0.0.0.0"),
    ("port", "6006"),
    ("format", "json"),
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

    NOTE: config file takes priority over CLI args if it exists

    -h, --help            Print this help message and exit
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
        if arg == "-h" || arg == "--help" {
            print!("{}", HELP);
            process::exit(0);
        }

        let key = arg
            .strip_prefix("--")
            .ok_or_else(|| format!("unknown argument: {arg}"))?;

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

    let mut settings: HashMap<String, String> = if settings_path.exists() {
        let raw = std::fs::read_to_string(&settings_path)?;
        println!("config loaded from {}", settings_path.display());
        toml::from_str(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
    } else {
        println!("no config found, writing defaults...");
        std::fs::create_dir_all(&root_path)?;
        HashMap::new()
    };

    let config_existed = settings_path.exists();

    // fill in any missing keys: config wins, then cli, then hardcoded default
    for (key, default) in DEFAULT_SETTINGS {
        settings.entry(key.to_string()).or_insert_with(|| {
            cli_overrides
                .get(*key)
                .cloned()
                .unwrap_or_else(|| default.to_string())
        });
    }

    settings.insert(
        "root-path".to_string(),
        root_path.to_string_lossy().to_string(),
    );

    if !config_existed {
        let raw = toml::to_string_pretty(&settings)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        std::fs::write(&settings_path, raw)?;
        println!("config written to {}", settings_path.display());
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
// pub fn get_u16(settings: &HashMap<String, String>, key: &str) -> u16 {
//     get(settings, key)
//         .parse()
//         .unwrap_or_else(|_| default_value(key).parse().unwrap_or(0))
// }
