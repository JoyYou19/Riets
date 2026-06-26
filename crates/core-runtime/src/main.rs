use core_core::{CorelamoDatabase, DatabaseOptions};
use std::{collections::HashMap, io, path::PathBuf, process};
const DEFAULT_ROOT: &str = "/var/lib/corelamo";
const DEFAULT_NAME: &str = "corelamo";
const HELP: &str = "\
corelamo-runtime 
USAGE:
    corelamo-runtime [OPTIONS]
OPTIONS:
    --root-path <path>    Root directory for all databases and config
                          [default: /var/lib/corelamo]

    --name <name>         Name of your database 
                          [default: corelamo]

    -h, --help            Print this help message and exit
";

struct Args {
    root: PathBuf,
    name: String,
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

            other => {
                return Err(format!("unknown argument: {other}"));
            }
        }
    }

    Ok(Args {
        root: resolve_root(root.unwrap_or_else(|| PathBuf::from(DEFAULT_ROOT))),
        name: name.unwrap_or_else(|| DEFAULT_NAME.to_string()),
    })
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct DatabaseSettings {
    name: String,
}

impl DatabaseSettings {
    fn new(name: String) -> Self {
        Self { name }
    }

    fn databases_dir(&self, root: &PathBuf) -> PathBuf {
        root.join("databases")
    }
}

fn load_or_init_settings(root: &PathBuf, name: String) -> io::Result<DatabaseSettings> {
    let settings_path = root.join("DatabaseSettings.toml");

    if settings_path.exists() {
        let raw = std::fs::read_to_string(&settings_path)?;
        let settings: DatabaseSettings =
            toml::from_str(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        println!("config loaded from {}", settings_path.display());
        return Ok(settings);
    }

    println!("no config found, writing defaults...");
    std::fs::create_dir_all(root)?;
    let settings = DatabaseSettings::new(name);
    let raw =
        toml::to_string_pretty(&settings).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    std::fs::write(&settings_path, raw)?;
    println!("config written to {}", settings_path.display());

    Ok(settings)
}

fn load_databases(databases_dir: &PathBuf) -> io::Result<HashMap<String, CorelamoDatabase>> {
    if !databases_dir.exists() {
        println!("databases dir not found, creating...");
        std::fs::create_dir_all(databases_dir)?;
        println!("no databases found");
        return Ok(HashMap::new());
    }

    let mut databases: HashMap<String, CorelamoDatabase>;
    databases = HashMap::new();

    for entry in std::fs::read_dir(databases_dir)? {
        let entry = entry?;
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        let db_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();

        //TODO: we need to save the database setting to disk and not use default
        match CorelamoDatabase::open(&path, DatabaseOptions::default()) {
            Ok(db) => {
                println!("opened database: {db_name}");
                databases.insert(db_name, db);
            }
            Err(e) => {
                eprintln!("warning: failed to open database '{db_name}': {e}");
            }
        }
    }

    Ok(databases)
}

fn main() -> io::Result<()> {
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

    let settings = match load_or_init_settings(&args.root, args.name) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error loading settings: {e}");
            process::exit(1);
        }
    };

    println!("name: {}", settings.name);

    let databases_dir = settings.databases_dir(&args.root);
    println!("databases_dir: {}", databases_dir.display());
    let databases = match load_databases(&databases_dir) {
        Ok(dbs) => dbs,
        Err(e) => {
            eprintln!("error loading databases: {e}");
            process::exit(1);
        }
    };

    //TODO: start/open only the databases that had bootable:yes
    println!("found and opened {} database(s)", databases.len());

    // HACK: making a temp_db for testing uncomment if needed
    // let db = CorelamoDatabase::open(
    //     &settings.databases_dir(&args.root).join("temp_db"),
    //     DatabaseOptions::default(),
    // )?;
    // println!("database root={}", db.root().display());
    // println!("policy file={}", db.policy_path().display());
    // println!("policy={:#?}", db.policy());
    // println!("stats={:#?}", db.stats()?);
    // db.shutdown().unwrap();

    Ok(())
}
