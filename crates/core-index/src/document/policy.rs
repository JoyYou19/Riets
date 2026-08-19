use std::{
    collections::{BTreeMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tantivy::schema::FieldType;

use crate::types::XPathId;

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

// Represents how we handle a single field inside of a file, for example for "title" we must have a
// weight from x to y and it must be index=true etc.
//TODO: the list would need to be some enum with yes/no/snippet right?
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldPolicy {
    pub name: String,
    //now automatic
    //pub xpath: XPathId,
    pub index: IndexKind,
    pub list: bool,
    pub weight: WeightInterval,
    pub stemming: Option<String>,
}

impl FieldPolicy {
    pub fn xpath(&self, policy: &IndexPolicy) -> XPathId {
        policy.xpath_of(&self.name).unwrap_or_else(|| {
            panic!(
                "field '{}' does not belong to the given IndexPolicy — \
                 xpath() must be called with the policy that owns this field",
                self.name
            )
        })
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
            .filter(|f| matches!(f.index, IndexKind::Id | IndexKind::IdAuto))
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
            FieldPolicy {
                name: "id".to_string(),
                weight: WeightInterval { min: 100, max: 100 },
                index: IndexKind::IdAuto,
                list: true,
                stemming: None,
            },
            FieldPolicy {
                name: "title".to_string(),
                weight: WeightInterval::TITLE,
                index: IndexKind::Text,
                list: true,
                stemming: Some("english".to_string()),
            },
            FieldPolicy {
                name: "body".to_string(),
                weight: WeightInterval::TEXT,
                index: IndexKind::Text,
                list: true,
                stemming: Some("english".to_string()),
            },
        ])
    }

    pub fn id_field(&self) -> Option<&FieldPolicy> {
        self.fields
            .iter()
            .find(|f| matches!(f.index, IndexKind::Id | IndexKind::IdAuto))
    }

    pub fn id_field_name(&self) -> Option<&str> {
        self.id_field().map(|f| f.name.as_str())
    }

    pub fn is_auto_increment(&self) -> bool {
        self.id_field()
            .map(|f| f.index == IndexKind::IdAuto)
            .unwrap_or(false)
    }

    pub fn indexed_fields(&self) -> impl Iterator<Item = &FieldPolicy> {
        self.fields
            .iter()
            .filter(|field| field.index != IndexKind::None)
    }

    pub fn xpath_of(&self, name: &str) -> Option<XPathId> {
        self.registry.get(name)
    }

    pub fn searchable_xpaths(&self) -> impl Iterator<Item = XPathId> + '_ {
        self.indexed_fields().map(move |field| {
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
        }
        if registry.len() != before {
            registry.save(&registry_path)?;
        }

        self.registry = registry;
        Ok(())
    }

    pub fn load(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref();
        let contents = fs::read_to_string(Self::policy_path(root))?;
        let mut policy: Self =
            toml::from_str(&contents).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        policy.validate()?;
        policy.resolve(root)?;
        Ok(policy)
    }

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

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub enum IndexKind {
    None,
    Text,
    Number,
    Date,
    Id,
    IdAuto,
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
//
//
//
//

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FieldRegistry {
    next_id: XPathId,
    ids: BTreeMap<String, XPathId>,
}

impl FieldRegistry {
    fn new() -> Self {
        Self {
            next_id: 1,
            ids: BTreeMap::new(),
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

        let max_assigned = registry.ids.values().copied().max().unwrap_or(0);
        registry.next_id = registry.next_id.max(max_assigned + 1);
        Ok(registry)
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
        self.ids.len()
    }
}

impl Default for FieldRegistry {
    fn default() -> Self {
        Self::new()
    }
}
