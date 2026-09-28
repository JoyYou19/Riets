use std::{
    collections::{BTreeMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
};

use crate::{
    numeric_values::{parse_float, parse_integer},
    types::XPathId,
};
use core_timing::timed;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeightInterval {
    pub min: u16,
    pub max: u16,
}

impl WeightInterval {
    pub const TITLE: Self = Self { min: 65, max: 90 };
    pub const TEXT: Self = Self { min: 1, max: 75 };
    pub const DEFAULT: Self = Self { min: 0, max: 100 };

    pub fn new(min: u16, max: u16) -> Self {
        assert!(min <= max, "weight interval min must be <= max");
        Self { min, max }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldPolicy {
    pub name: String,
    pub kind: FieldKind,
    pub searchable: bool,
    pub list: bool,
    pub weight: WeightInterval,
    pub stemming: Option<String>,
    pub exact: bool,
    pub array: bool,
    pub subfields: Vec<FieldPolicy>,
    pub full_path: String,
    pub row_keyed: bool,
    pub depth: u32,
}

impl Serialize for FieldPolicy {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;

        let defaults = self.kind.defaults();
        let mut st = serializer.serialize_struct("FieldPolicy", 9)?;

        st.serialize_field("name", &self.name)?;
        st.serialize_field("kind", &self.kind)?;

        if self.searchable != defaults.searchable {
            st.serialize_field("searchable", &self.searchable)?;
        }
        if self.list != defaults.list {
            st.serialize_field("list", &self.list)?;
        }
        if self.weight != defaults.weight {
            st.serialize_field("weight", &self.weight)?;
        }
        if let Some(stem) = &self.stemming {
            st.serialize_field("stemming", stem)?;
        }
        if self.exact != defaults.exact {
            st.serialize_field("exact", &self.exact)?;
        }
        if self.array {
            st.serialize_field("array", &self.array)?;
        }
        if !self.subfields.is_empty() {
            st.serialize_field("subfields", &self.subfields)?;
        }

        st.end()
    }
}

impl FieldPolicy {
    pub fn new(name: impl Into<String>, kind: FieldKind) -> Self {
        Self::from_raw(RawFieldPolicy {
            name: name.into(),
            kind,
            searchable: None,
            list: None,
            weight: None,
            stemming: None,
            exact: None,
            array: None,
            subfields: None,
        })
    }

    pub fn xpath(&self, policy: &IndexPolicy) -> XPathId {
        policy
            .xpath_of(&self.full_path)
            .unwrap_or_else(|| panic!("field '{}' has no registered xpath", self.full_path))
    }

    pub fn searchable(&self) -> bool {
        self.searchable
    }

    pub fn has_exact_index(&self) -> bool {
        self.kind == FieldKind::Text && self.exact
    }

    pub fn exact_xpath(&self, policy: &IndexPolicy) -> Option<XPathId> {
        if !self.has_exact_index() {
            return None;
        }
        Some(policy.exact_xpath_of(&self.full_path).unwrap_or_else(|| {
            panic!(
                "field '{}' has no exact xpath — IndexPolicy::resolve() should have registered it",
                self.full_path
            )
        }))
    }

    // Yields the field itself if it's an indexed leaf, else recurses into Array subfields.
    fn collect_leaves<'a>(&'a self, out: &mut Vec<&'a FieldPolicy>) {
        if self.kind == FieldKind::Array {
            for sub in &self.subfields {
                sub.collect_leaves(out);
            }
        } else if self.kind != FieldKind::None {
            out.push(self);
        }
    }

    fn from_raw(raw: RawFieldPolicy) -> Self {
        let defaults = raw.kind.defaults();
        Self {
            name: raw.name.clone(),
            kind: raw.kind,
            searchable: raw.searchable.unwrap_or(defaults.searchable),
            list: raw.list.unwrap_or(defaults.list),
            weight: raw.weight.unwrap_or(defaults.weight),
            stemming: raw.stemming,
            exact: raw.exact.unwrap_or(defaults.exact),
            array: raw.array.unwrap_or(false),
            subfields: raw.subfields.unwrap_or_default(),
            full_path: raw.name,
            row_keyed: false,
            depth: 0,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFieldPolicy {
    name: String,
    kind: FieldKind,
    searchable: Option<bool>,
    list: Option<bool>,
    weight: Option<WeightInterval>,
    stemming: Option<String>,
    exact: Option<bool>,
    array: Option<bool>,
    subfields: Option<Vec<FieldPolicy>>,
}

impl<'de> Deserialize<'de> for FieldPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawFieldPolicy::deserialize(deserializer)?;
        Ok(Self::from_raw(raw))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexPolicy {
    pub fields: Vec<FieldPolicy>,
    #[serde(skip)]
    registry: FieldRegistry,
}

impl IndexPolicy {
    pub const POLICY_FILE_NAME: &'static str = "policy.toml";
    pub const FULL_FILE_NAME: &'static str = "policy.full";
    pub const REGISTRY_FILE_NAME: &'static str = "xpath_registry.toml";

    fn policy_path(root: &Path) -> PathBuf {
        root.join(Self::POLICY_FILE_NAME)
    }

    fn full_path(root: &Path) -> PathBuf {
        root.join(Self::FULL_FILE_NAME)
    }

    fn registry_path(root: &Path) -> PathBuf {
        root.join(Self::REGISTRY_FILE_NAME)
    }

    pub fn new(fields: Vec<FieldPolicy>) -> Self {
        Self {
            fields,
            registry: FieldRegistry::new(),
        }
    }

    pub fn from_toml(contents: &str) -> io::Result<Self> {
        let policy: Self =
            toml::from_str(contents).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(policy)
    }

    pub fn leaf_by_path(&self, path: &str) -> Option<&FieldPolicy> {
        fn find<'a>(fields: &'a [FieldPolicy], path: &str) -> Option<&'a FieldPolicy> {
            for f in fields {
                if f.kind == FieldKind::Array {
                    if let Some(hit) = find(&f.subfields, path) {
                        return Some(hit);
                    }
                } else if f.full_path == path {
                    return Some(f);
                }
            }
            None
        }
        find(&self.fields, path)
    }

    //validates if everything is fine for policy
    pub fn validate(&self) -> io::Result<()> {
        let mut names = HashSet::new();

        for field in &self.fields {
            if !names.insert(field.name.clone()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("duplicate field name '{}'", field.name),
                ));
            }

            //TODO: id shouldnt neeed a weight right?
            if field.weight.min > field.weight.max {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid weight range for '{}'", field.name),
                ));
            }
        }

        let id_count = self
            .fields
            .iter()
            .filter(|f| matches!(f.kind, FieldKind::Id | FieldKind::IdAuto))
            .count();
        if id_count != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("policy must have exactly one id field, found {id_count}"),
            ));
        }

        for field in &self.fields {
            validate_field(field, 0)?;
        }

        Ok(())
    }

    pub fn default_document() -> Self {
        Self::new(vec![
            FieldPolicy::new("id", FieldKind::IdAuto),
            FieldPolicy {
                weight: WeightInterval::TITLE,
                stemming: Some("english".to_string()),
                ..FieldPolicy::new("title", FieldKind::Text)
            },
            FieldPolicy {
                stemming: Some("english".to_string()),
                ..FieldPolicy::new("body", FieldKind::Text)
            },
        ])
    }

    pub fn id_field(&self) -> Option<&FieldPolicy> {
        self.fields
            .iter()
            .find(|f| matches!(f.kind, FieldKind::Id | FieldKind::IdAuto))
    }

    pub fn indexed_fields(&self) -> Vec<&FieldPolicy> {
        let mut out = Vec::new();
        for f in &self.fields {
            f.collect_leaves(&mut out);
        }
        out
    }

    pub fn has_hidden_fields(&self) -> bool {
        self.indexed_fields().into_iter().any(|f| !f.list)
    }

    pub fn xpath_of(&self, name: &str) -> Option<XPathId> {
        self.registry.get(name)
    }

    pub fn exact_xpath_of(&self, name: &str) -> Option<XPathId> {
        self.registry.get_exact(name)
    }

    pub fn searchable_xpaths(&self) -> Vec<XPathId> {
        self.indexed_fields()
            .into_iter()
            .filter(|field| !field.kind.is_numeric())
            .map(|field| field.xpath(self))
            .collect()
    }

    pub fn exact_xpaths(&self) -> Vec<XPathId> {
        self.indexed_fields()
            .into_iter()
            .filter(|field| field.has_exact_index())
            .map(|field| field.exact_xpath(self).unwrap())
            .collect()
    }

    pub fn resolve(&mut self, root: impl AsRef<Path>) -> io::Result<()> {
        let root = root.as_ref();
        let registry_path = Self::registry_path(root);
        let mut registry = FieldRegistry::load(&registry_path)?;

        let before = registry.len();
        for field in &mut self.fields {
            stamp_field(field, "", 0, &mut registry);
        }
        if registry.len() != before {
            registry.save(&registry_path)?;
        }

        self.registry = registry;
        Ok(())
    }

    #[timed(database_lifecycle)]
    pub fn load(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref();
        let contents = fs::read_to_string(Self::policy_path(root))?;
        let mut policy: Self =
            toml::from_str(&contents).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        policy.validate()?;
        policy.resolve(root)?;
        Ok(policy)
    }

    #[timed(writing_files)]
    pub fn save(&mut self, root: impl AsRef<Path>) -> io::Result<()> {
        let root = root.as_ref();
        self.validate()?;
        self.resolve(root)?;

        // minimal (hide defaults) -> policy.toml
        let minimal = toml::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(Self::policy_path(root), minimal)?;

        // full resolved (defaults + stamps) -> policy.full
        let full = toml::to_string_pretty(&Full(self))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(Self::full_path(root), full)
    }

    pub fn write_default(root: impl AsRef<Path>) -> io::Result<()> {
        let mut policy = Self::default_document();
        policy.save(root)
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum FieldKind {
    None,
    Text,
    Integer,
    Float,
    Date,
    Id,
    IdAuto,
    Array,
}

#[derive(Debug, Clone, Copy)]
struct FieldDefaults {
    searchable: bool,
    list: bool,
    weight: WeightInterval,
    exact: bool,
}

//reasonable default for each type of field based on my intuition
impl FieldKind {
    fn defaults(self) -> FieldDefaults {
        match self {
            FieldKind::Text => FieldDefaults {
                searchable: true,
                list: true,
                weight: WeightInterval::TEXT,
                exact: false,
            },
            FieldKind::Id => FieldDefaults {
                searchable: true,
                list: true,
                weight: WeightInterval::DEFAULT,
                exact: true,
            },
            FieldKind::IdAuto => FieldDefaults {
                searchable: false,
                list: true,
                weight: WeightInterval::DEFAULT,
                exact: true,
            },
            FieldKind::Integer | FieldKind::Float | FieldKind::Date => FieldDefaults {
                searchable: false,
                list: true,
                weight: WeightInterval::DEFAULT,
                exact: true,
            },
            FieldKind::Array => FieldDefaults {
                searchable: false,
                list: true,
                weight: WeightInterval::DEFAULT,
                exact: false,
            },
            FieldKind::None => FieldDefaults {
                searchable: false,
                list: true,
                weight: WeightInterval::DEFAULT,
                exact: false,
            },
        }
    }

    pub fn is_numeric(self) -> bool {
        matches!(self, FieldKind::Integer | FieldKind::Float)
    }

    pub fn label(self) -> &'static str {
        match self {
            FieldKind::None => "none",
            FieldKind::Text => "text",
            FieldKind::Integer => "integer",
            FieldKind::Float => "float",
            FieldKind::Date => "date",
            FieldKind::Id => "id",
            FieldKind::IdAuto => "id",
            FieldKind::Array => "array",
        }
    }

    pub fn validate_value(self, raw: &str) -> Result<(), String> {
        let valid = match self {
            FieldKind::Integer => parse_integer(raw).is_some(),
            FieldKind::Float => parse_float(raw).is_some(),
            _ => true,
        };
        if valid {
            Ok(())
        } else {
            Err(format!("expected {}, got '{}'", self.label(), raw.trim()))
        }
    }
}

pub enum MatchMode {
    FullText,
    Exact,
    Both,
}

//Array inside Array inside Array... max
const MAX_ARRAY_DEPTH: u32 = 3;

//arrays should have one subfield + max 3 + numbers cant be exact=false
fn validate_field(field: &FieldPolicy, array_depth: u32) -> io::Result<()> {
    if field.kind == FieldKind::Array {
        let depth = array_depth + 1;
        if field.subfields.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("array '{}' must declare at least one subfield", field.name),
            ));
        }
        if depth > MAX_ARRAY_DEPTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "field '{}' nests deeper than {MAX_ARRAY_DEPTH} array levels",
                    field.name
                ),
            ));
        }
        for sub in &field.subfields {
            validate_field(sub, depth)?;
        }
    } else if field.kind.is_numeric() && !field.exact {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "numeric field '{}' must have exact = true (or omit it)",
                field.name
            ),
        ));
    }

    Ok(())
}

fn stamp_field(
    field: &mut FieldPolicy,
    prefix: &str,
    array_depth: u32,
    registry: &mut FieldRegistry,
) {
    let full = if prefix.is_empty() {
        field.name.clone()
    } else {
        format!("{prefix}/{}", field.name)
    };
    field.full_path = full.clone();

    if field.kind == FieldKind::Array {
        let d = array_depth + 1;
        for sub in &mut field.subfields {
            stamp_field(sub, &full, d, registry);
        }
    } else {
        field.row_keyed = field.array || array_depth > 0;
        field.depth = if field.array { 1 } else { array_depth };
        registry.resolve(&full);
        if field.has_exact_index() {
            registry.resolve_exact(&full);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FieldRegistry {
    next_id: XPathId,
    ids: BTreeMap<String, XPathId>,
    #[serde(default)]
    exact_ids: BTreeMap<String, XPathId>,
}

impl FieldRegistry {
    fn new() -> Self {
        Self {
            next_id: 1,
            ids: BTreeMap::new(),
            exact_ids: BTreeMap::new(),
        }
    }

    fn load(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::new());
        }
        let contents = fs::read_to_string(path)?;
        let mut registry: Self =
            toml::from_str(&contents).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        //porniks ar tiem exact logic
        let max_assigned = registry.ids.values().copied().max().unwrap_or(0);
        let max_exact = registry.exact_ids.values().copied().max().unwrap_or(0);

        registry.next_id = registry.next_id.max(max_assigned + 1).max(max_exact + 1);
        Ok(registry)
    }

    fn resolve_exact(&mut self, name: &str) -> XPathId {
        if let Some(&id) = self.exact_ids.get(name) {
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.exact_ids.insert(name.to_string(), id);
        id
    }

    fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        let contents = toml::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp_path = path.with_file_name(format!(
            "{}.tmp",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        fs::write(&tmp_path, contents)?;
        fs::rename(&tmp_path, path)?;
        Ok(())
    }

    fn resolve(&mut self, name: &str) -> XPathId {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.ids.insert(name.to_string(), id);
        id
    }

    fn get(&self, name: &str) -> Option<XPathId> {
        self.ids.get(name).copied()
    }

    fn len(&self) -> usize {
        self.ids.len() + self.exact_ids.len()
    }

    fn get_exact(&self, name: &str) -> Option<XPathId> {
        self.exact_ids.get(name).copied()
    }
}

impl Default for FieldRegistry {
    fn default() -> Self {
        Self::new()
    }
}

//helper to write the pretty + full policy
struct Full<'a>(&'a IndexPolicy);
impl Serialize for Full<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("IndexPolicy", 1)?;
        st.serialize_field("fields", &FullFields(&self.0.fields))?;
        st.end()
    }
}

struct FullFields<'a>(&'a [FieldPolicy]);

impl Serialize for FullFields<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for f in self.0 {
            seq.serialize_element(&FullField(f))?;
        }
        seq.end()
    }
}

struct FullField<'a>(&'a FieldPolicy);
impl Serialize for FullField<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let f = self.0;
        let mut st = s.serialize_struct("FieldPolicy", 12)?;
        st.serialize_field("name", &f.name)?;
        st.serialize_field("kind", &f.kind)?;
        st.serialize_field("searchable", &f.searchable)?;
        st.serialize_field("list", &f.list)?;
        st.serialize_field("weight", &f.weight)?;
        if let Some(stem) = &f.stemming {
            st.serialize_field("stemming", stem)?;
        }
        st.serialize_field("exact", &f.exact)?;
        st.serialize_field("array", &f.array)?;
        st.serialize_field("subfields", &FullFields(&f.subfields))?;
        st.serialize_field("full_path", &f.full_path)?;
        st.serialize_field("row_keyed", &f.row_keyed)?;
        st.serialize_field("depth", &f.depth)?;
        st.end()
    }
}
