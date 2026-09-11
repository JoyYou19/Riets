use std::{
    collections::{BTreeMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
};

use crate::types::XPathId;
use core_timing::timed;
use serde::{Deserialize, Serialize};

// Core policy, eventually will need to move to a configuration file
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

//TODO: the list would need to be some enum with yes/no/snippet right?
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FieldPolicy {
    pub name: String,
    pub kind: FieldKind,
    pub searchable: bool,
    pub list: bool,
    pub weight: WeightInterval,
    pub stemming: Option<String>,
    pub exact: bool,
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
        })
    }

    pub fn xpath(&self, policy: &IndexPolicy) -> XPathId {
        policy.xpath_of(&self.name).unwrap_or_else(|| {
            panic!(
                "field '{}' does not belong to the given IndexPolicy — \
                 xpath() must be called with the policy that owns this field",
                self.name
            )
        })
    }

    pub fn searchable(&self) -> bool {
        self.searchable
    }

    pub fn list(&self) -> bool {
        self.list
    }

    pub fn weight(&self) -> WeightInterval {
        self.weight
    }

    pub fn stemming(&self) -> Option<&str> {
        self.stemming.as_deref()
    }

    pub fn exact(&self) -> bool {
        self.exact
    }

    pub fn has_column(&self) -> bool {
        self.kind.has_column()
    }

    pub fn has_exact_index(&self) -> bool {
        self.kind == FieldKind::Text && self.exact
    }

    pub fn exact_xpath(&self, policy: &IndexPolicy) -> Option<XPathId> {
        if !self.has_exact_index() {
            return None;
        }
        Some(policy.exact_xpath_of(&self.name).unwrap_or_else(|| {
            panic!(
                "field '{}' has no exact xpath — IndexPolicy::resolve() should have registered it",
                self.name
            )
        }))
    }

    fn from_raw(raw: RawFieldPolicy) -> Self {
        let defaults = raw.kind.defaults();
        Self {
            name: raw.name,
            kind: raw.kind,
            searchable: raw.searchable.unwrap_or(defaults.searchable),
            list: raw.list.unwrap_or(defaults.list),
            weight: raw.weight.unwrap_or(defaults.weight),
            stemming: raw.stemming,
            exact: raw.exact.unwrap_or(defaults.exact),
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
    pub const REGISTRY_FILE_NAME: &'static str = "xpath_registry.toml";

    fn policy_path(root: &Path) -> PathBuf {
        root.join(Self::POLICY_FILE_NAME)
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

    pub fn indexed_fields(&self) -> impl Iterator<Item = &FieldPolicy> {
        self.fields
            .iter()
            .filter(|field| field.kind != FieldKind::None)
    }

    pub fn xpath_of(&self, name: &str) -> Option<XPathId> {
        self.registry.get(name)
    }

    pub fn exact_xpath_of(&self, name: &str) -> Option<XPathId> {
        self.registry.get_exact(name)
    }

    pub fn searchable_xpaths(&self) -> impl Iterator<Item = XPathId> + '_ {
        self.indexed_fields()
            .filter(|field| !field.kind.is_numeric())
            .map(move |field| {
                self.registry.get(&field.name).unwrap_or_else(|| {
                    panic!(
                        "field '{}' has no registered xpath id IndexPolicy::load()/save()/resolve() should happen",
                        field.name
                    )
                })
            })
    }

    pub fn resolve(&mut self, root: impl AsRef<Path>) -> io::Result<()> {
        let root = root.as_ref();
        let registry_path = Self::registry_path(root);
        let mut registry = FieldRegistry::load(&registry_path)?;

        let before = registry.len();
        for field in &self.fields {
            registry.resolve(&field.name);
            if field.has_exact_index() {
                registry.resolve_exact(&field.name);
            }
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
        let contents = toml::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        fs::write(Self::policy_path(root), contents)
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
            FieldKind::None => FieldDefaults {
                searchable: false,
                list: false,
                weight: WeightInterval::DEFAULT,
                exact: false,
            },
        }
    }

    pub fn is_numeric(self) -> bool {
        matches!(self, FieldKind::Integer | FieldKind::Float)
    }

    pub fn has_column(self) -> bool {
        matches!(
            self,
            FieldKind::Integer | FieldKind::Float | FieldKind::Date
        )
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
        }
    }

    pub fn validate_value(self, raw: &str) -> Result<(), String> {
        let valid = match self {
            FieldKind::Integer => raw.trim().parse::<i64>().is_ok(),
            FieldKind::Float => raw
                .trim()
                .parse::<f64>()
                .map(|value| value.is_finite())
                .unwrap_or(false),
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

//
//
//
//
//

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

    fn resolve_exact(&mut self, name: &str) -> XPathId {
        if let Some(&id) = self.exact_ids.get(name) {
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.exact_ids.insert(name.to_string(), id);
        id
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
