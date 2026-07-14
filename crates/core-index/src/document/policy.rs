//TODO: Valters uztaisi ka var nested fields, luuuuuuuuuuuuuuudzu

use std::{collections::HashSet, fs, io, path::Path};

use serde::{Deserialize, Serialize};

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
    pub xpath: XPathId,
    pub index: IndexKind,
    pub list: bool,
    pub weight: WeightInterval,
    pub stemming: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexPolicy {
    pub fields: Vec<FieldPolicy>,
}

impl IndexPolicy {
    pub fn new(fields: Vec<FieldPolicy>) -> Self {
        Self { fields }
    }

    // Validates if each field of the policy is fine, might be missing some
    pub fn validate(&self) -> io::Result<()> {
        let mut names = HashSet::new();
        let mut xpaths = HashSet::new();

        for field in &self.fields {
            if !names.insert(field.name.clone()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("duplicate field name '{}'", field.name),
                ));
            }

            if !xpaths.insert(field.xpath) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("duplicate xpath '{}'", field.xpath),
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

        //there must be only one field with id/id-auto
        let id_count = self
            .fields
            .iter()
            .filter(|f| matches!(f.index, IndexKind::Id | IndexKind::IdAutoIncrement))
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
                xpath: 1,
                weight: WeightInterval { min: 100, max: 100 },
                index: IndexKind::Id,
                list: true,
                stemming: None,
            },
            FieldPolicy {
                name: "title".to_string(),
                xpath: 2,
                weight: WeightInterval::TITLE,
                index: IndexKind::Text,
                list: true,
                stemming: Some("english".to_string()),
            },
            FieldPolicy {
                name: "body".to_string(),
                xpath: 3,
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
            .find(|f| matches!(f.index, IndexKind::Id | IndexKind::IdAutoIncrement))
    }

    pub fn id_field_name(&self) -> Option<&str> {
        self.id_field().map(|f| f.name.as_str())
    }

    pub fn is_auto_increment(&self) -> bool {
        self.id_field()
            .map(|f| f.index == IndexKind::IdAutoIncrement)
            .unwrap_or(false)
    }

    pub fn indexed_fields(&self) -> impl Iterator<Item = &FieldPolicy> {
        self.fields
            .iter()
            .filter(|field| field.index != IndexKind::None)
    }

    pub fn searchable_xpaths(&self) -> impl Iterator<Item = XPathId> + '_ {
        self.indexed_fields().map(|field| field.xpath)
    }

    // Loads a policy file from a file path
    pub fn load(path: impl AsRef<Path>) -> io::Result<Self> {
        let contents = fs::read_to_string(path)?;

        let policy: Self =
            toml::from_str(&contents).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        policy.validate()?;

        Ok(policy)
    }

    // Saves itself to a file path as a toml entry
    pub fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let contents = toml::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        fs::write(path, contents)
    }

    // Writes to a disk a default policy
    pub fn write_default(path: impl AsRef<Path>) -> io::Result<()> {
        Self::default_document().save(path)
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub enum IndexKind {
    None,
    Text,
    Number,
    Date,
    Id,
    IdAutoIncrement,
}

pub enum MatchMode {
    FullText,
    Exact,
    Both,
}
