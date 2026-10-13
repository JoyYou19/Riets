use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use crate::{
    numeric_values::{parse_float, parse_integer},
    types::XPathId,
};
use ahash::HashSet;
use core_timing::timed;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

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
    pub repeated: bool, // array of strings nums/bools

    pub subfields: Vec<FieldPolicy>,
    pub full_path: String,
    pub row_keyed: bool,
    pub depth: u32,
}

impl Serialize for FieldPolicy {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let defaults = self.kind.defaults();
        let mut m = s.serialize_map(None)?;

        if self.kind != FieldKind::Struct {
            m.serialize_entry("$type", &self.kind)?;
        }
        if self.searchable != defaults.searchable {
            m.serialize_entry("$searchable", &self.searchable)?;
        }
        if self.list != defaults.list {
            m.serialize_entry("$list", &self.list)?;
        }
        if self.weight != defaults.weight {
            m.serialize_entry("$weight", &self.weight)?;
        }
        if let Some(stem) = &self.stemming {
            m.serialize_entry("$stemming", stem)?;
        }
        if self.exact != defaults.exact {
            m.serialize_entry("$exact", &self.exact)?;
        }
        if self.repeated {
            m.serialize_entry("$repeated", &self.repeated)?;
        }
        for sub in &self.subfields {
            m.serialize_entry(&sub.name, sub)?;
        }
        m.end()
    }
}

impl FieldPolicy {
    pub fn new(name: impl Into<String>, kind: FieldKind) -> Self {
        let name = name.into();
        Self::leaf(&name, kind)
    }

    fn leaf(name: &str, kind: FieldKind) -> Self {
        let defaults = kind.defaults();
        Self {
            name: name.to_string(),
            kind,
            searchable: defaults.searchable,
            list: defaults.list,
            weight: defaults.weight,
            stemming: None,
            exact: defaults.exact,
            repeated: false,
            subfields: Vec::new(),
            full_path: name.to_string(),
            row_keyed: false,
            depth: 0,
        }
    }

    fn struct_with(name: &str, subfields: Vec<FieldPolicy>) -> Self {
        let defaults = FieldKind::Struct.defaults();
        Self {
            name: name.to_string(),
            kind: FieldKind::Struct,
            searchable: defaults.searchable,
            list: defaults.list,
            weight: defaults.weight,
            stemming: None,
            exact: defaults.exact,
            repeated: false,
            subfields,
            full_path: name.to_string(),
            row_keyed: false,
            depth: 0,
        }
    }

    fn from_json(name: &str, v: &Value) -> Result<Self, String> {
        match v {
            Value::String(s) => {
                let kind = kind_from_str(s)
                    .ok_or_else(|| format!("field '{name}': unknown type '{s}'"))?;
                Ok(Self::leaf(name, kind))
            }

            Value::Object(obj) => match obj.get("$type").and_then(Value::as_str) {
                Some("struct") => Self::struct_from_obj(name, obj),
                Some(t) => Self::leaf_from_obj(name, t, obj),
                None => Self::struct_from_obj(name, obj),
            },

            Value::Array(items) => {
                let elem = items
                    .first()
                    .ok_or_else(|| format!("repeated field '{name}' needs exactly one element"))?;
                let mut f = Self::from_json(name, elem)?;
                f.repeated = true;
                Ok(f)
            }

            other => Err(format!(
                "field '{name}': expected a string, object or array, got {}",
                json_kind_name(other)
            )),
        }
    }

    fn leaf_from_obj(
        name: &str,
        type_str: &str,
        obj: &serde_json::Map<String, Value>,
    ) -> Result<Self, String> {
        let kind = kind_from_str(type_str)
            .ok_or_else(|| format!("field '{name}': unknown type '{type_str}'"))?;
        let mut f = Self::leaf(name, kind);
        if let Some(e) = obj.get("$exact") {
            f.exact = e
                .as_bool()
                .ok_or_else(|| format!("field '{name}': $exact must be a bool"))?;
        }
        if let Some(b) = obj.get("$searchable") {
            f.searchable = b
                .as_bool()
                .ok_or_else(|| format!("field '{name}': $searchable must be a bool"))?;
        }
        if let Some(b) = obj.get("$list") {
            f.list = b
                .as_bool()
                .ok_or_else(|| format!("field '{name}': $list must be a bool"))?;
        }
        if let Some(b) = obj.get("$repeated") {
            f.repeated = b
                .as_bool()
                .ok_or_else(|| format!("field '{name}': $repeated must be a bool"))?;
        }
        if let Some(stem) = obj.get("$stemming") {
            f.stemming = Some(
                stem.as_str()
                    .ok_or_else(|| format!("field '{name}': $stemming must be a string"))?
                    .to_string(),
            );
        }
        if let Some(w) = obj.get("$weight") {
            f.weight = parse_weight(w).map_err(|e| format!("field '{name}': $weight: {e}"))?;
        }
        Ok(f)
    }

    fn struct_from_obj(name: &str, obj: &serde_json::Map<String, Value>) -> Result<Self, String> {
        let mut subfields = Vec::new();
        for (k, val) in obj {
            if k.starts_with('$') {
                continue;
            }
            subfields.push(Self::from_json(k, val)?);
        }
        let mut f = Self::struct_with(name, subfields);
        if let Some(w) = obj.get("$weight") {
            f.weight = parse_weight(w).map_err(|e| format!("field '{name}': $weight: {e}"))?;
        }
        Ok(f)
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
        //either text+exact or Id IdAuto
        (self.kind == FieldKind::Text && self.exact)
            || matches!(self.kind, FieldKind::Id | FieldKind::IdAuto)
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
    pub fn collect_leaves<'a>(&'a self, out: &mut Vec<&'a FieldPolicy>) {
        if self.kind == FieldKind::Struct {
            for sub in &self.subfields {
                sub.collect_leaves(out);
            }
        } else if self.kind != FieldKind::None {
            out.push(self);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexPolicy {
    pub fields: Vec<FieldPolicy>,
    registry: FieldRegistry,
}

// Bare-object serialization: field names are the top-level keys.
impl Serialize for IndexPolicy {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(self.fields.len()))?;
        for f in &self.fields {
            m.serialize_entry(&f.name, f)?;
        }
        m.end()
    }
}

impl<'de> Deserialize<'de> for IndexPolicy {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        let obj = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("policy must be a JSON object"))?;

        let mut fields = Vec::with_capacity(obj.len());
        for (name, v) in obj {
            fields.push(FieldPolicy::from_json(name, v).map_err(serde::de::Error::custom)?);
        }
        Ok(IndexPolicy::new(fields))
    }
}

impl IndexPolicy {
    pub const POLICY_FILE_NAME: &'static str = "policy.json";
    pub const FULL_FILE_NAME: &'static str = "policy.json.full";
    pub const REGISTRY_FILE_NAME: &'static str = "xpath_registry.json";

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

    pub fn array_groups(&self) -> Vec<Vec<(XPathId, Option<XPathId>)>> {
        let mut groups = Vec::new();
        for field in &self.fields {
            if field.kind == FieldKind::Struct {
                let mut subs = Vec::new();
                Self::collect_subfield_contexts(field, &mut subs, self);
                groups.push(subs);
            }
        }
        groups
    }

    fn collect_subfield_contexts(
        field: &FieldPolicy,
        out: &mut Vec<(XPathId, Option<XPathId>)>,
        policy: &IndexPolicy,
    ) {
        for sub in &field.subfields {
            if sub.kind == FieldKind::Struct {
                Self::collect_subfield_contexts(sub, out, policy);
            } else {
                out.push((sub.xpath(policy), sub.exact_xpath(policy)));
            }
        }
    }

    pub fn from_json(contents: &str) -> io::Result<Self> {
        serde_json::from_str(contents).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn leaf_by_path(&self, path: &str) -> Option<&FieldPolicy> {
        fn find<'a>(fields: &'a [FieldPolicy], path: &str) -> Option<&'a FieldPolicy> {
            for f in fields {
                if f.kind == FieldKind::Struct {
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

    // validates if everything is fine for policy
    pub fn validate(&self) -> io::Result<()> {
        let mut names = HashSet::default();

        for field in &self.fields {
            if !names.insert(field.name.clone()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("duplicate field name '{}'", field.name),
                ));
            }

            // TODO: id shouldnt need a weight right?
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

    pub fn searchable_fields(&self) -> Vec<&FieldPolicy> {
        self.indexed_fields()
            .into_iter()
            .filter(|field| field.searchable() && !field.row_keyed)
            .collect()
    }

    pub fn searchable_xpaths(&self) -> Vec<XPathId> {
        self.searchable_fields()
            .into_iter()
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

    fn write_full(&self, root: impl AsRef<Path>) -> io::Result<()> {
        let full = serde_json::to_string_pretty(&Full(self))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(Self::full_path(root.as_ref()), full)
    }

    #[timed(database_lifecycle)]
    pub fn load(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref();
        let contents = fs::read_to_string(Self::policy_path(root))?;
        let mut policy: Self = serde_json::from_str(&contents)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        policy.validate()?;
        policy.resolve(root)?;
        policy.write_full(root)?; // keep policy.json.full in sync
        Ok(policy)
    }

    pub fn field_by_path(&self, path: &str) -> Option<&FieldPolicy> {
        fn find<'a>(fields: &'a [FieldPolicy], path: &str) -> Option<&'a FieldPolicy> {
            for f in fields {
                if f.full_path == path {
                    return Some(f);
                }
                if f.kind == FieldKind::Struct {
                    if let Some(hit) = find(&f.subfields, path) {
                        return Some(hit);
                    }
                }
            }
            None
        }
        find(&self.fields, path)
    }

    #[timed(writing_files)]
    pub fn save(&mut self, root: impl AsRef<Path>) -> io::Result<()> {
        let root = root.as_ref();
        self.validate()?;
        self.resolve(root)?;

        // minimal (hide defaults) -> policy.json
        let minimal = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(Self::policy_path(root), minimal)?;

        // full resolved (defaults + stamps) -> policy.json.full
        let full = serde_json::to_string_pretty(&Full(self))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(Self::full_path(root), full)
    }

    pub fn write_default(root: impl AsRef<Path>) -> io::Result<()> {
        let mut policy = Self::default_document();
        policy.save(root)
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum FieldKind {
    None,
    Text,
    Integer,
    Float,
    Date,
    Id,
    IdAuto,
    Struct, // list of objects
    Bool,
}

impl Serialize for FieldKind {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.label())
    }
}

impl<'de> Deserialize<'de> for FieldKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        kind_from_str(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown field type '{s}'")))
    }
}

#[derive(Debug, Clone, Copy)]
struct FieldDefaults {
    searchable: bool,
    list: bool,
    weight: WeightInterval,
    exact: bool,
}

// reasonable default for each type of field based on my intuition
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
            FieldKind::Struct => FieldDefaults {
                searchable: false,
                list: true,
                weight: WeightInterval::DEFAULT,
                exact: false,
            },
            FieldKind::Bool => FieldDefaults {
                searchable: false,
                list: true,
                weight: WeightInterval::DEFAULT,
                exact: true,
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

    pub fn is_bool(self) -> bool {
        matches!(self, FieldKind::Bool)
    }

    pub fn label(self) -> &'static str {
        match self {
            FieldKind::None => "none",
            FieldKind::Text => "text",
            FieldKind::Integer => "int",
            FieldKind::Float => "float",
            FieldKind::Date => "date",
            FieldKind::Id => "id",
            FieldKind::IdAuto => "id_auto",
            FieldKind::Struct => "struct",
            FieldKind::Bool => "bool",
        }
    }

    pub fn validate_value(self, raw: &str) -> Result<(), String> {
        let valid = match self {
            FieldKind::Integer => parse_integer(raw).is_some(),
            FieldKind::Float => parse_float(raw).is_some(),
            FieldKind::Bool => matches!(raw.trim(), "true" | "false"),
            _ => true,
        };
        if valid {
            Ok(())
        } else {
            Err(format!("expected {}, got '{}'", self.label(), raw.trim()))
        }
    }
}

fn kind_from_str(s: &str) -> Option<FieldKind> {
    match s {
        "none" => Some(FieldKind::None),
        "text" => Some(FieldKind::Text),
        "int" | "integer" => Some(FieldKind::Integer),
        "float" => Some(FieldKind::Float),
        "date" => Some(FieldKind::Date),
        "id" => Some(FieldKind::Id),
        "id_auto" => Some(FieldKind::IdAuto),
        "struct" => Some(FieldKind::Struct),
        "bool" => Some(FieldKind::Bool),
        _ => None,
    }
}

fn parse_weight(v: &Value) -> Result<WeightInterval, String> {
    serde_json::from_value(v.clone()).map_err(|e| e.to_string())
}

fn json_kind_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

pub enum MatchMode {
    FullText,
    Exact,
    Both,
}

// Array inside Array inside Array... max
const MAX_ARRAY_DEPTH: u32 = 3;

// arrays should have one subfield + max 3 + numbers cant be exact=false
fn validate_field(field: &FieldPolicy, array_depth: u32) -> io::Result<()> {
    if field.kind == FieldKind::Struct {
        let depth = array_depth + 1;
        if field.subfields.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Struct '{}' must declare at least one subfield", field.name),
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

    if field.kind == FieldKind::Struct {
        let d = array_depth + 1;
        field.depth = d;
        for sub in &mut field.subfields {
            stamp_field(sub, &full, d, registry);
        }
    } else {
        field.row_keyed = field.repeated || array_depth > 0;
        field.depth = if field.repeated { 1 } else { array_depth };
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
        let mut registry: Self = serde_json::from_str(&contents)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

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
        let contents = serde_json::to_string_pretty(self)
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

// helper to write the pretty + full policy
struct Full<'a>(&'a IndexPolicy);
impl Serialize for Full<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(self.0.fields.len()))?;
        for f in &self.0.fields {
            m.serialize_entry(&f.name, &FullField(f))?;
        }
        m.end()
    }
}

struct FullField<'a>(&'a FieldPolicy);
impl Serialize for FullField<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let f = self.0;
        let mut m = s.serialize_map(None)?;
        if f.kind != FieldKind::Struct {
            m.serialize_entry("$type", &f.kind)?;
        }
        m.serialize_entry("$searchable", &f.searchable)?;
        m.serialize_entry("$list", &f.list)?;
        m.serialize_entry("$weight", &f.weight)?;
        if let Some(stem) = &f.stemming {
            m.serialize_entry("$stemming", stem)?;
        }
        m.serialize_entry("$exact", &f.exact)?;
        m.serialize_entry("$repeated", &f.repeated)?;
        m.serialize_entry("$full_path", &f.full_path)?;
        m.serialize_entry("$row_keyed", &f.row_keyed)?;
        m.serialize_entry("$depth", &f.depth)?;
        for sub in &f.subfields {
            m.serialize_entry(&sub.name, &FullField(sub))?;
        }
        m.end()
    }
}
