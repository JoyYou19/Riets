

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use serde::Deserialize;
use toml::Spanned;

use crate::syn::{parse_variant, DictionaryError, RomanConfig, SynonymDictionary, SynonymStore};

/// Seed for `CorelamoDictionary.toml`: `default.toml` in the workspace root,
/// compiled into the binary so a fresh data root gets a full dictionary.
pub const DEFAULT_DICTIONARY: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../default.toml"));
pub const BUILTIN_NAME: &str = "<built-in default>";

/// The template's file name in the data root, beside CorelamoSettings.toml.
pub const DEFAULT_FILE_NAME: &str = "CorelamoDictionary.toml";

/// Each database's dictionary, inside that database's directory.
pub const DATABASE_FILE_NAME: &str = "dictionary.toml";

/// Written by `start_empty`: no groups and no Roman numeral expansion.
pub const EMPTY_DICTIONARY: &str = "\
# Synonym dictionary for this database. It is empty: nothing is expanded.
[roman]
enabled = false
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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DictionaryFile {
    #[serde(default)]
    roman: RomanSection,
    #[serde(default)]
    acronyms: GroupSection,
    #[serde(default)]
    formulas: GroupSection,
    #[serde(default)]
    synonyms: GroupSection,
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
pub struct DictionaryDocument {
    name: String,
    path: Option<PathBuf>,
    text: String,
    file: DictionaryFile,
    disk_fingerprint: Option<u64>,
}

impl DictionaryDocument {
    /// Parses TOML and checks its shape. Group contents and Roman values are
    /// checked by `compile`, which every save path runs first.
    pub fn parse(name: impl Into<String>, text: &str) -> Result<Self, ConfigError> {
        let name = name.into();
        let file: DictionaryFile = toml::from_str(text).map_err(|err| ConfigError::Syntax {
            line: err.span().map(|span| line_of(text, span.start)),
            message: err.message().to_owned(),
            source_name: name.clone(),
        })?;
        Ok(Self { name, path: None, text: text.to_owned(), file, disk_fingerprint: None })
    }

    pub fn builtin_default() -> Result<Self, ConfigError> {
        Self::parse(BUILTIN_NAME, DEFAULT_DICTIONARY)
    }

    /// A missing file loads as an empty document bound to `path` with
    /// `exists_on_disk() == false`.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
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
                let mut doc = Self::parse(path.display().to_string(), "")?;
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

    /// Takes over another document's file identity (name, path, last-seen state).
    fn bound_to(mut self, target: &DictionaryDocument) -> Self {
        self.name = target.name.clone();
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
}

/// Validates a document and builds its dictionary.
pub fn compile(doc: &DictionaryDocument) -> Result<SynonymDictionary, ConfigError> {
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
    for section in [&file.acronyms, &file.formulas, &file.synonyms] {
        for group in &section.groups {
            //line numbers are only worked out for an error: counting lines per group is O(n^2)
            let error = |source| ConfigError::Dictionary {
                source_name: doc.name.clone(),
                line: span_line(&doc.text, group.span()),
                source,
            };
            let variants = group
                .get_ref()
                .iter()
                .map(|spec| parse_variant(spec.trim()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(error)?;
            builder.add_group(variants).map_err(error)?;
        }
    }
    builder.roman(RomanConfig {
        enabled: roman.enabled,
        min_value: roman.min_value,
        max_value: roman.max_value,
        min_len: roman.min_len,
        arabic_to_roman: roman.arabic_to_roman,
        denylist,
    });
    Ok(builder.build())
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

/// Loads `path`, first writing the built-in dictionary there if it is missing.
fn load_or_seed(path: &Path) -> Result<DictionaryDocument, ConfigError> {
    let doc = DictionaryDocument::load(path)?;
    if doc.exists_on_disk() {
        return Ok(doc);
    }
    let mut seeded = DictionaryDocument::builtin_default()?.bound_to(&doc);
    seeded.save()?;
    Ok(seeded)
}

/// Shares one compiled dictionary between databases whose files are
/// identical (every unedited copy of the template). Entries die with their
/// last user.
#[derive(Debug, Default)]
struct CompiledCache {
    entries: Mutex<HashMap<(u64, usize), Weak<SynonymDictionary>>>,
}

impl CompiledCache {
    fn get_or_compile(&self, doc: &DictionaryDocument) -> Result<Arc<SynonymDictionary>, ConfigError> {
        let key = (fingerprint(doc.text.as_bytes()), doc.text.len());
        let mut entries = lock(&self.entries);
        if let Some(shared) = entries.get(&key).and_then(Weak::upgrade) {
            return Ok(shared);
        }
        let compiled = Arc::new(compile(doc)?);
        entries.retain(|_, weak| weak.strong_count() > 0);
        entries.insert(key, Arc::downgrade(&compiled));
        Ok(compiled)
    }
}

// ---------------------------------------------------------------- registry

/// Entry point for the server: the template next to CorelamoSettings.toml,
/// and each database's own dictionary.
#[derive(Debug)]
pub struct SynonymRegistry {
    template_path: PathBuf,
    //serializes template writes against copies taken from it
    template_lock: Mutex<()>,
    cache: Arc<CompiledCache>,
}

impl SynonymRegistry {
    /// `data_root` is the directory holding CorelamoSettings.toml. Writes the
    /// template from the built-in copy if it is missing, and fails (with file
    /// and line) if an existing template is invalid.
    pub fn open(data_root: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let template_path = data_root.as_ref().join(DEFAULT_FILE_NAME);
        let template = load_or_seed(&template_path)?;
        compile(&template)?;
        Ok(Self { template_path, template_lock: Mutex::new(()), cache: Arc::default() })
    }

    pub fn template_path(&self) -> &Path {
        &self.template_path
    }

    /// The template as it is on disk right now (re-seeded if deleted).
    fn read_template(&self) -> Result<DictionaryDocument, ConfigError> {
        let template = load_or_seed(&self.template_path)?;
        compile(&template)?;
        Ok(template)
    }

    pub fn template_text(&self) -> Result<String, ConfigError> {
        let _guard = lock(&self.template_lock);
        Ok(self.read_template()?.text)
    }

    /// Opens `<database_dir>/dictionary.toml`, first copying the current
    /// template in if the database has none. An existing file is never touched.
    pub fn open_database(&self, database_dir: impl AsRef<Path>) -> Result<SynonymManager, ConfigError> {
        let path = database_dir.as_ref().join(DATABASE_FILE_NAME);
        let mut file = DictionaryDocument::load(&path)?;
        if !file.exists_on_disk() {
            let _guard = lock(&self.template_lock);
            let mut copy = self.read_template()?.bound_to(&file);
            copy.save()?;
            file = copy;
        }
        let dictionary = self.cache.get_or_compile(&file)?;
        Ok(SynonymManager {
            template_path: self.template_path.clone(),
            cache: Arc::clone(&self.cache),
            file: Mutex::new(file),
            store: SynonymStore::from_arc(dictionary),
        })
    }

    /// Replaces the template. Existing databases keep their own copies.
    pub fn replace_template_text(&self, text: &str) -> Result<(), ConfigError> {
        let _guard = lock(&self.template_lock);
        let current = DictionaryDocument::load(&self.template_path)?;
        let mut draft = DictionaryDocument::parse(current.name.clone(), text)?.bound_to(&current);
        compile(&draft)?;
        draft.save()
    }

    /// Overwrites the template with the dictionary compiled into this binary.
    pub fn restore_builtin_template(&self) -> Result<(), ConfigError> {
        self.replace_template_text(DEFAULT_DICTIONARY)
    }
}

// ---------------------------------------------------------------- per database

/// One database's dictionary: always its own `dictionary.toml`.
#[derive(Debug)]
pub struct SynonymManager {
    template_path: PathBuf,
    cache: Arc<CompiledCache>,
    file: Mutex<DictionaryDocument>,
    store: SynonymStore,
}

impl SynonymManager {
    /// Take once per request and share it with every shard.
    pub fn snapshot(&self) -> Arc<SynonymDictionary> {
        self.store.snapshot()
    }

    pub fn path(&self) -> Option<PathBuf> {
        lock(&self.file).path().map(Path::to_path_buf)
    }

    // /// The file exactly as it is in use, comments included.
    pub fn config_text(&self) -> String {
        lock(&self.file).text.clone()
    }
    pub fn replace_document(&self, doc: DictionaryDocument) -> Result<(), ConfigError> {
        let mut file = lock(&self.file);
        self.commit(&mut file, doc)
    }

    /// Re-reads the file after a hand edit or a restore. A deleted file is
    /// copied from the template again. On any error the live dictionary stays.
    pub fn reload(&self) -> Result<(), ConfigError> {
        let mut file = lock(&self.file);
        let mut fresh = DictionaryDocument::load(file.require_path()?)?;
        if !fresh.exists_on_disk() {
            let mut copy = load_or_seed(&self.template_path)?.bound_to(&fresh);
            copy.save()?;
            fresh = copy;
        }
        let dictionary = self.cache.get_or_compile(&fresh)?;
        self.store.replace_arc(dictionary);
        *file = fresh;
        Ok(())
    }

    /// Replaces this database's dictionary with the given TOML. It is
    /// validated before it is saved or used; on error nothing changes.
    pub fn replace_text(&self, text: &str) -> Result<(), ConfigError> {
        let mut file = lock(&self.file);
        let draft = DictionaryDocument::parse(file.name.clone(), text)?;
        self.commit(&mut file, draft)
    }

    /// Gives this database an empty dictionary (no synonyms, no numerals).
    pub fn start_empty(&self) -> Result<(), ConfigError> {
        self.replace_text(EMPTY_DICTIONARY)
    }

    /// Overwrites this database's dictionary with the current template.
    pub fn reset_to_template(&self) -> Result<(), ConfigError> {
        let template = load_or_seed(&self.template_path)?;
        let mut file = lock(&self.file);
        self.commit(&mut file, template)
    }

    fn commit(&self, file: &mut DictionaryDocument, draft: DictionaryDocument) -> Result<(), ConfigError> {
        let mut draft = draft.bound_to(file);
        let dictionary = self.cache.get_or_compile(&draft)?;
        draft.save()?;
        self.store.replace_arc(dictionary);
        *file = draft;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syn::QueryNode;

    fn term(s: &str) -> QueryNode {
        QueryNode::Term(s.to_owned())
    }

    fn phrase(words: &[&str]) -> QueryNode {
        QueryNode::Phrase(words.iter().map(|w| (*w).to_owned()).collect())
    }

    fn exact(words: &[&str]) -> QueryNode {
        QueryNode::Exact(words.iter().map(|w| (*w).to_owned()).collect())
    }

    fn any(nodes: Vec<QueryNode>) -> Vec<QueryNode> {
        vec![QueryNode::AnyOf(nodes)]
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "synonym-toml-{tag}-{}-{}",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const SMALL_TEMPLATE: &str = "\
# small template for tests
[formulas]
groups = [
  [\"CaP\", \"calcium phosphate\"],
]
";

    fn registry(tag: &str) -> (PathBuf, SynonymRegistry) {
        let root = temp_dir(tag);
        fs::write(root.join(DEFAULT_FILE_NAME), SMALL_TEMPLATE).unwrap();
        let registry = SynonymRegistry::open(&root).unwrap();
        (root, registry)
    }

    fn cap_expansion() -> Vec<QueryNode> {
        any(vec![exact(&["CaP"]), phrase(&["calcium", "phosphate"])])
    }

    #[test]
    fn builtin_default_compiles() {
        let dict = compile(&DictionaryDocument::builtin_default().unwrap()).unwrap();
        assert_eq!(dict.expand_query("H2O"), any(vec![term("h2o"), term("water")]));
        assert_eq!(dict.expand_query("cap"), vec![term("cap")]);
    }

    #[test]
    fn omitted_settings_use_defaults() {
        let empty = compile(&DictionaryDocument::parse("t", "").unwrap()).unwrap();
        let builtin = compile(&DictionaryDocument::builtin_default().unwrap()).unwrap();
        for query in ["I", "II", "IV", "MIX", "XL", "MMXXVI", "1", "2", "40", "2026", "C"] {
            assert_eq!(empty.expand_number(query), builtin.expand_number(query), "query {query}");
        }
    }

    #[test]
    fn errors_name_the_line() {
        let syntax = DictionaryDocument::parse("db", "[acronyms]\ngroups = [\n  [\"HA\" \"x\"],\n]\n").unwrap_err();
        assert!(matches!(syntax, ConfigError::Syntax { line: Some(3), .. }), "{syntax}");

        let unknown = DictionaryDocument::parse("db", "[acronym]\ngroups = []\n").unwrap_err();
        assert!(matches!(unknown, ConfigError::Syntax { line: Some(1), .. }), "{unknown}");

        let doc = DictionaryDocument::parse("db", "[acronyms]\ngroups = [\n  [\"HA\", \"hydroxyapatite\"],\n  [\"PVP\"],\n]\n").unwrap();
        let small = compile(&doc).unwrap_err();
        assert!(matches!(small, ConfigError::Dictionary { line: Some(4), .. }), "{small}");

        let doc = DictionaryDocument::parse("db", "[roman]\nmin_value = 50\nmax_value = 10\n").unwrap();
        assert!(matches!(compile(&doc), Err(ConfigError::Invalid { .. })));

        let doc = DictionaryDocument::parse("db", "[roman]\ndenylist = [\"IV\",\n  \"ABC\"]\n").unwrap();
        assert!(matches!(compile(&doc), Err(ConfigError::Syntax { line: Some(3), .. })));
    }

    #[test]
    fn roman_settings_apply() {
        let doc = DictionaryDocument::parse("t", "[roman]\narabic_to_roman = false\ndenylist = [\"iv\"]\n").unwrap();
        let dict = compile(&doc).unwrap();
        assert_eq!(dict.expand_query("2"), vec![term("2")]);
        assert_eq!(dict.expand_query("IV"), vec![term("iv")]);
        assert_eq!(dict.expand_query("XL"), any(vec![term("xl"), term("40")]));
    }

    #[test]
    fn first_start_writes_builtin_template() {
        let root = temp_dir("seed");
        let registry = SynonymRegistry::open(&root).unwrap();
        assert_eq!(fs::read_to_string(root.join(DEFAULT_FILE_NAME)).unwrap(), DEFAULT_DICTIONARY);
        assert_eq!(registry.template_text().unwrap(), DEFAULT_DICTIONARY);
        let _again = SynonymRegistry::open(&root).unwrap();
        assert_eq!(fs::read_to_string(root.join(DEFAULT_FILE_NAME)).unwrap(), DEFAULT_DICTIONARY);
    }

    #[test]
    fn broken_template_fails_with_line_number() {
        let root = temp_dir("broken");
        fs::write(root.join(DEFAULT_FILE_NAME), "[acronyms]\ngroups = [\n  [\"HA\", \"hydroxyapatite\"],\n  [\"PVP\"],\n]\n").unwrap();
        let err = SynonymRegistry::open(&root).unwrap_err();
        assert!(matches!(err, ConfigError::Dictionary { line: Some(4), .. }), "{err}");
    }

    #[test]
    fn new_database_gets_a_copy_and_later_template_edits_only_reach_new_ones() {
        let (root, registry) = registry("copy");
        let old_dir = root.join("databases").join("old");
        let old = registry.open_database(&old_dir).unwrap();
        assert_eq!(fs::read_to_string(old_dir.join(DATABASE_FILE_NAME)).unwrap(), SMALL_TEMPLATE);
        assert_eq!(old.snapshot().expand_query("CaP"), cap_expansion());

        registry
            .replace_template_text("[acronyms]\ngroups = [[\"HA\", \"hydroxyapatite\"]]\n")
            .unwrap();
        let new = registry.open_database(root.join("databases").join("new")).unwrap();
        assert_eq!(old.snapshot().expand_query("HA"), vec![term("ha")]);
        assert_eq!(new.snapshot().expand_query("HA"), any(vec![exact(&["HA"]), term("hydroxyapatite")]));

        old.reset_to_template().unwrap();
        assert_eq!(old.snapshot().expand_query("HA"), any(vec![exact(&["HA"]), term("hydroxyapatite")]));
        assert!(Arc::ptr_eq(&old.snapshot(), &new.snapshot()));
    }

    #[test]
    fn invalid_template_replacement_changes_nothing() {
        let (root, registry) = registry("bad-template");
        assert!(registry.replace_template_text("[acronyms]\ngroups = [[\"HA\"]]\n").is_err());
        assert_eq!(fs::read_to_string(root.join(DEFAULT_FILE_NAME)).unwrap(), SMALL_TEMPLATE);
    }

    #[test]
    fn replace_keeps_text_exactly_and_invalid_changes_nothing() {
        let (root, registry) = registry("replace");
        let dir = root.join("databases").join("db");
        let db = registry.open_database(&dir).unwrap();
        let path = dir.join(DATABASE_FILE_NAME);

        let text = "# my notes\n[acronyms]\ngroups = [\n  [\"HA\", \"hydroxyapatite\"],  # bone\n]\n";
        db.replace_text(text).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        // assert_eq!(db.config_text(), text);

        let before = db.snapshot();
        assert!(db.replace_text("[acronyms]\ngroups = [[\"HA\"]]\n").is_err());
        assert!(db.replace_text("not toml at all [").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        assert!(Arc::ptr_eq(&before, &db.snapshot()));

        db.start_empty().unwrap();
        assert_eq!(db.snapshot().expand_query("HA"), vec![term("ha")]);
        assert_eq!(db.snapshot().expand_query("II"), vec![term("ii")]);
    }

    #[test]
    fn identical_copies_share_one_compiled_dictionary() {
        let (root, registry) = registry("shared");
        let a = registry.open_database(root.join("databases").join("a")).unwrap();
        let b = registry.open_database(root.join("databases").join("b")).unwrap();
        assert!(Arc::ptr_eq(&a.snapshot(), &b.snapshot()));
        b.replace_text("[synonyms]\ngroups = [[\"big\", \"large\"]]\n").unwrap();
        assert!(!Arc::ptr_eq(&a.snapshot(), &b.snapshot()));
    }

    #[test]
    fn hand_edit_conflicts_until_reloaded_and_deleted_file_is_recopied() {
        let (root, registry) = registry("conflict");
        let dir = root.join("databases").join("db");
        let db = registry.open_database(&dir).unwrap();
        let path = dir.join(DATABASE_FILE_NAME);

        fs::write(&path, "[acronyms]\ngroups = [[\"PEG\", \"polyethylene glycol\"]]\n").unwrap();
        let err = db.replace_text("[acronyms]\ngroups = [[\"HA\", \"hydroxyapatite\"]]\n").unwrap_err();
        assert!(matches!(err, ConfigError::Conflict { .. }));

        db.reload().unwrap();
        assert_eq!(
            db.snapshot().expand_query("PEG"),
            any(vec![exact(&["PEG"]), phrase(&["polyethylene", "glycol"])])
        );

        fs::write(&path, "[acronyms]\ngroups = [[\"HA\"]]\n").unwrap();
        assert!(db.reload().is_err());
        assert_eq!(db.snapshot().expand_query("PEG").len(), 1);

        fs::remove_file(&path).unwrap();
        db.reload().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), SMALL_TEMPLATE);
        assert_eq!(db.snapshot().expand_query("CaP"), cap_expansion());
    }
}