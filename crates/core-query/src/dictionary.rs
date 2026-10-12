//! The dictionary files. `CorelamoDictionary.toml`, next to CorelamoSettings.toml, is the default
//! every database can search with: short forms, formulas and the Roman numeral rules. Each
//! database's `dictionary.toml` says whether it uses that default and adds its own synonyms and
//! one-way entries.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use toml::Spanned;

use core_index::analyzer::Analyzer;

use crate::syn::{
    DictionaryBuilder, DictionaryError, RomanConfig, SynonymDictionary, SynonymStore, Synonyms,
    parse_variant,
};

/// Seed for `CorelamoDictionary.toml`: `default.toml` in the workspace root, compiled into the
/// binary so a fresh data root gets the full default.
pub const DEFAULT_DICTIONARY: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../default.toml"));

/// The default dictionary's file name in the data root, beside CorelamoSettings.toml.
pub const DEFAULT_FILE_NAME: &str = "CorelamoDictionary.toml";

/// Each database's dictionary, inside that database's directory.
pub const DATABASE_FILE_NAME: &str = "dictionary.toml";

/// What a new database's dictionary.toml starts as.
pub const NEW_DATABASE_DICTIONARY: &str = "\
# This database's synonyms, searched together with the default dictionary
# (CorelamoDictionary.toml next to CorelamoSettings.toml: short forms, formulas, Roman numerals).
# default = false searches with the groups below only.
default = true

[synonyms]
# Forms that mean the same: any of them finds the others. A form written with a capital
# only matches when typed exactly like that; `~` in front makes it match any case.
groups = [
  # [\"car\", \"automobile\"],
]

[one_way]
# The first form also searches the others; the others do not search it.
groups = [
  # [\"king\", \"monarch\"],
]
";

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum ConfigError {
    Io { path: PathBuf, source: io::Error },
    /// Not valid TOML, or a section/setting that doesn't exist.
    Syntax { source_name: String, line: Option<usize>, message: String },
    /// A group that can't be used (too few forms, a form with no words or too many).
    Dictionary { source_name: String, line: Option<usize>, source: DictionaryError },
    /// Valid shape, impossible values (min_value above max_value, ...).
    Invalid { message: String },
    /// The file changed on disk since it was loaded; reload before writing.
    Conflict { path: PathBuf },
    NoPath { source_name: String },
}

fn location(source_name: &str, line: Option<usize>) -> String {
    match line {
        Some(line) => format!("{source_name}:{line}"),
        None => source_name.to_owned(),
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Syntax { source_name, line, message } => {
                write!(f, "{}: {message}", location(source_name, *line))
            }
            Self::Dictionary { source_name, line, source } => {
                write!(f, "{}: {source}", location(source_name, *line))
            }
            Self::Invalid { message } => write!(f, "invalid dictionary: {message}"),
            Self::Conflict { path } => write!(
                f,
                "{} changed on disk since it was loaded; reload before editing",
                path.display()
            ),
            Self::NoPath { source_name } => write!(f, "{source_name} has no file path"),
        }
    }
}

impl From<ConfigError> for core_protocol::errors::CorelamoError {
    fn from(err: ConfigError) -> Self {
        use core_protocol::errors::CorelamoError;
        match err {
            ConfigError::Io { .. } => CorelamoError::Internal(err.to_string()),
            _ => CorelamoError::InvalidData(err.to_string()),
        }
    }
}
// ---------------------------------------------------------------- file schema

/// `CorelamoDictionary.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultFile {
    #[serde(default)]
    roman: RomanSection,
    #[serde(default)]
    acronyms: GroupSection,
    #[serde(default)]
    formulas: GroupSection,
    #[serde(default)]
    synonyms: GroupSection,
    #[serde(default)]
    one_way: GroupSection,
}

/// A database's `dictionary.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseFile {
    /// Whether the database also searches with the default dictionary.
    #[serde(default = "uses_default")]
    default: bool,
    #[serde(default)]
    synonyms: GroupSection,
    #[serde(default)]
    one_way: GroupSection,
}

fn uses_default() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupSection {
    #[serde(default)]
    groups: Vec<Spanned<Vec<String>>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RomanSection {
    enabled: bool,
    min_value: u16,
    max_value: u16,
    min_len: usize,
    arabic_to_roman: bool,
    denylist: Vec<Spanned<String>>,
}

impl Default for RomanSection {
    fn default() -> Self {
        let defaults = RomanConfig::default();
        let mut denylist: Vec<String> = defaults.denylist.into_iter().collect();
        denylist.sort();
        Self {
            enabled: defaults.enabled,
            min_value: defaults.min_value,
            max_value: defaults.max_value,
            min_len: defaults.min_len,
            arabic_to_roman: defaults.arabic_to_roman,
            denylist: denylist.into_iter().map(|item| Spanned::new(0..0, item)).collect(),
        }
    }
}

fn line_of(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.min(text.len())].iter().filter(|&&b| b == b'\n').count() + 1
}

fn span_line(text: &str, span: Range<usize>) -> Option<usize> {
    (span.end > 0).then(|| line_of(text, span.start))
}

fn fingerprint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

// ---------------------------------------------------------------- document

/// One dictionary file: its exact text plus the parsed form.
#[derive(Debug, Clone)]
pub struct Document<F> {
    name: String,
    path: Option<PathBuf>,
    text: String,
    file: F,
    disk_fingerprint: Option<u64>,
}

/// A database's `dictionary.toml`.
pub type DictionaryDocument = Document<DatabaseFile>;

impl<F: DeserializeOwned> Document<F> {
    /// Parses TOML and checks its shape. The groups and Roman values are checked by compiling,
    /// which every save runs first.
    pub fn parse(name: impl Into<String>, text: &str) -> Result<Self, ConfigError> {
        let name = name.into();
        let file: F = toml::from_str(text).map_err(|err| ConfigError::Syntax {
            line: err.span().map(|span| line_of(text, span.start)),
            message: err.message().to_owned(),
            source_name: name.clone(),
        })?;
        Ok(Self { name, path: None, text: text.to_owned(), file, disk_fingerprint: None })
    }

    /// A missing file loads as `missing` bound to `path` with `exists_on_disk() == false`.
    fn load(path: &Path, missing: &str) -> Result<Self, ConfigError> {
        match fs::read(path) {
            Ok(bytes) => {
                let text = String::from_utf8(bytes).map_err(|err| ConfigError::Io {
                    path: path.to_path_buf(),
                    source: io::Error::new(io::ErrorKind::InvalidData, err),
                })?;
                let mut doc = Self::parse(path.display().to_string(), &text)?;
                doc.path = Some(path.to_path_buf());
                doc.disk_fingerprint = Some(fingerprint(text.as_bytes()));
                Ok(doc)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let mut doc = Self::parse(path.display().to_string(), missing)?;
                doc.path = Some(path.to_path_buf());
                Ok(doc)
            }
            Err(source) => Err(ConfigError::Io { path: path.to_path_buf(), source }),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The file exactly as written, comments included.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn exists_on_disk(&self) -> bool {
        self.disk_fingerprint.is_some()
    }

    fn require_path(&self) -> Result<PathBuf, ConfigError> {
        self.path
            .clone()
            .ok_or_else(|| ConfigError::NoPath { source_name: self.name.clone() })
    }

    /// Takes over another document's file (path and last-seen state). The name stays: errors
    /// in a dictionary sent over the API name the database, not a path on the server.
    fn bound_to(mut self, target: &Self) -> Self {
        self.path = target.path.clone();
        self.disk_fingerprint = target.disk_fingerprint;
        self
    }

    /// Atomic save that refuses to overwrite changes made on disk since load.
    fn save(&mut self) -> Result<(), ConfigError> {
        let path = self.require_path()?;
        check_unchanged(&path, self.disk_fingerprint)?;
        write_atomic(&path, self.text.as_bytes())
            .map_err(|source| ConfigError::Io { path: path.clone(), source })?;
        self.disk_fingerprint = Some(fingerprint(self.text.as_bytes()));
        Ok(())
    }

    /// Loads `path`, first writing `seed` there if it is missing.
    fn load_or_seed(path: &Path, seed: &str) -> Result<Self, ConfigError> {
        let doc = Self::load(path, seed)?;
        if !doc.exists_on_disk() {
            let mut seeded = Self::parse(doc.name.clone(), seed)?.bound_to(&doc);
            seeded.save()?;
            return Ok(seeded);
        }
        Ok(doc)
    }
}

// ---------------------------------------------------------------- compiling

/// Validates the default dictionary and builds it, keyed by the forms `analyzer` gives words.
pub fn compile_default(
    doc: &Document<DefaultFile>,
    analyzer: &Analyzer,
) -> Result<SynonymDictionary, ConfigError> {
    let file = &doc.file;
    let roman = &file.roman;
    let invalid = |message: String| ConfigError::Invalid { message: format!("{}: {message}", doc.name) };
    for (key, value) in [("min_value", roman.min_value), ("max_value", roman.max_value)] {
        if !(1..=3999).contains(&value) {
            return Err(invalid(format!("[roman] {key} must be in 1..=3999, got {value}")));
        }
    }
    if roman.min_value > roman.max_value {
        return Err(invalid(format!(
            "[roman] min_value ({}) exceeds max_value ({})",
            roman.min_value, roman.max_value
        )));
    }
    if !(1..=15).contains(&roman.min_len) {
        return Err(invalid(format!("[roman] min_len must be in 1..=15, got {}", roman.min_len)));
    }
    let mut denylist = std::collections::HashSet::with_capacity(roman.denylist.len());
    for item in &roman.denylist {
        let upper = item.get_ref().trim().to_ascii_uppercase();
        if upper.is_empty() || !upper.chars().all(|c| "IVXLCDM".contains(c)) {
            return Err(ConfigError::Syntax {
                source_name: doc.name.clone(),
                line: span_line(&doc.text, item.span()),
                message: format!("[roman] denylist entry {:?} is not a Roman numeral", item.get_ref()),
            });
        }
        denylist.insert(upper);
    }

    let mut builder = SynonymDictionary::builder();
    let sections = [
        (&file.acronyms, false),
        (&file.formulas, false),
        (&file.synonyms, false),
        (&file.one_way, true),
    ];
    add_sections(&mut builder, &doc.name, &doc.text, &sections)?;
    builder.roman(RomanConfig {
        enabled: roman.enabled,
        min_value: roman.min_value,
        max_value: roman.max_value,
        min_len: roman.min_len,
        arabic_to_roman: roman.arabic_to_roman,
        denylist,
    });
    Ok(builder.build(analyzer))
}

/// Validates a database's own groups and builds them. Roman numerals come with the default.
pub fn compile_database(
    doc: &DictionaryDocument,
    analyzer: &Analyzer,
) -> Result<SynonymDictionary, ConfigError> {
    let mut builder = SynonymDictionary::builder();
    let sections = [(&doc.file.synonyms, false), (&doc.file.one_way, true)];
    add_sections(&mut builder, &doc.name, &doc.text, &sections)?;
    builder.roman(RomanConfig {
        enabled: false,
        ..RomanConfig::default()
    });
    Ok(builder.build(analyzer))
}

fn add_sections(
    builder: &mut DictionaryBuilder,
    name: &str,
    text: &str,
    sections: &[(&GroupSection, bool)],
) -> Result<(), ConfigError> {
    for &(section, one_way) in sections {
        for group in &section.groups {
            //line numbers are only worked out for an error: counting lines per group is O(n^2)
            let error = |source| ConfigError::Dictionary {
                source_name: name.to_owned(),
                line: span_line(text, group.span()),
                source,
            };
            let variants = group
                .get_ref()
                .iter()
                .map(|spec| parse_variant(spec.trim()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(error)?;
            if one_way {
                builder.add_one_way(variants).map_err(error)?;
            } else {
                builder.add_group(variants).map_err(error)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- disk helpers

fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = parent_dir(path);
    fs::create_dir_all(&dir)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    sync_dir(&dir)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Fails with `Conflict` unless the file on disk is exactly what was loaded
/// (`None` = the file was absent and must still be absent).
fn check_unchanged(path: &Path, expected: Option<u64>) -> Result<(), ConfigError> {
    let on_disk = match fs::read(path) {
        Ok(bytes) => Some(fingerprint(&bytes)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(source) => return Err(ConfigError::Io { path: path.to_path_buf(), source }),
    };
    if on_disk == expected {
        Ok(())
    } else {
        Err(ConfigError::Conflict { path: path.to_path_buf() })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------- registry

/// Entry point for the server: the default dictionary next to CorelamoSettings.toml, compiled
/// once and shared by every database that uses it.
pub struct SynonymRegistry {
    template_path: PathBuf,
    analyzer: Analyzer,
    default: Arc<SynonymDictionary>,
}

impl fmt::Debug for SynonymRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SynonymRegistry")
            .field("template_path", &self.template_path)
            .finish_non_exhaustive()
    }
}

impl SynonymRegistry {
    /// `data_root` is the directory holding CorelamoSettings.toml. Writes the default dictionary
    /// from the built-in copy if it is missing, and fails (with file and line) if it is invalid.
    /// Changes to the file take effect when the server starts.
    pub fn open(data_root: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let template_path = data_root.as_ref().join(DEFAULT_FILE_NAME);
        let analyzer = Analyzer::new();
        let template = Document::<DefaultFile>::load_or_seed(&template_path, DEFAULT_DICTIONARY)?;
        let default = Arc::new(compile_default(&template, &analyzer)?);
        Ok(Self { template_path, analyzer, default })
    }

    pub fn template_path(&self) -> &Path {
        &self.template_path
    }

    /// Opens `<database_dir>/dictionary.toml`, writing a new one (using the default, no groups of
    /// its own) if the database has none.
    pub fn open_database(&self, database_dir: impl AsRef<Path>) -> Result<SynonymManager, ConfigError> {
        let path = database_dir.as_ref().join(DATABASE_FILE_NAME);
        let file = DictionaryDocument::load_or_seed(&path, NEW_DATABASE_DICTIONARY)?;
        let manager = SynonymManager {
            default: Arc::clone(&self.default),
            analyzer: self.analyzer.clone(),
            store: SynonymStore::new(Synonyms::default()),
            file: Mutex::new(file),
        };
        let synonyms = manager.synonyms(&lock(&manager.file))?;
        manager.store.replace(synonyms);
        Ok(manager)
    }
}

// ---------------------------------------------------------------- per database

/// One database's synonyms: its own `dictionary.toml`, and the default when that says so.
pub struct SynonymManager {
    default: Arc<SynonymDictionary>,
    analyzer: Analyzer,
    file: Mutex<DictionaryDocument>,
    store: SynonymStore,
}

impl fmt::Debug for SynonymManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SynonymManager")
            .field("path", &self.path())
            .finish_non_exhaustive()
    }
}

impl SynonymManager {
    /// Take once per request and share it with every shard.
    pub fn snapshot(&self) -> Arc<Synonyms> {
        self.store.snapshot()
    }

    pub fn path(&self) -> Option<PathBuf> {
        lock(&self.file).path().map(Path::to_path_buf)
    }

    /// This database's dictionary.toml as it is in use, comments included (not the default).
    pub fn config_text(&self) -> String {
        lock(&self.file).text.clone()
    }

    /// Replaces this database's dictionary.toml. It is validated before it is saved or used; on
    /// any error nothing changes.
    pub fn replace_document(&self, doc: DictionaryDocument) -> Result<(), ConfigError> {
        let mut file = lock(&self.file);
        let mut draft = doc.bound_to(&file);
        let synonyms = self.synonyms(&draft)?;
        draft.save()?;
        self.store.replace(synonyms);
        *file = draft;
        Ok(())
    }

    /// Re-reads dictionary.toml after a hand edit or a restore; a deleted file is written new.
    /// On any error the synonyms in use stay.
    pub fn reload(&self) -> Result<(), ConfigError> {
        let mut file = lock(&self.file);
        let fresh = DictionaryDocument::load_or_seed(&file.require_path()?, NEW_DATABASE_DICTIONARY)?;
        let synonyms = self.synonyms(&fresh)?;
        self.store.replace(synonyms);
        *file = fresh;
        Ok(())
    }

    fn synonyms(&self, doc: &DictionaryDocument) -> Result<Synonyms, ConfigError> {
        let own = Arc::new(compile_database(doc, &self.analyzer)?);
        Ok(Synonyms::new(if doc.file.default {
            vec![Arc::clone(&self.default), own]
        } else {
            vec![own]
        }))
    }
}
