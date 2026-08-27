use core_timing::timed;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use super::policy::{IndexKind, IndexPolicy};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AllFields {
    //xpath -> IndexKind if not found them None
    fields: BTreeMap<String, IndexKind>,
}

impl AllFields {
    pub const FILE_NAME: &'static str = "all_fields.toml";

    fn path(root: &Path) -> PathBuf {
        root.join(Self::FILE_NAME)
    }

    pub fn new() -> Self {
        Self::default()
    }

    #[timed(database_lifecycle)]
    pub fn load(root: impl AsRef<Path>) -> io::Result<Self> {
        let path = Self::path(root.as_ref());
        if !path.exists() {
            return Ok(Self::new());
        }
        let contents = fs::read_to_string(path)?;

        toml::from_str(&contents).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    #[timed(writing_files)]
    pub fn save(&self, root: impl AsRef<Path>) -> io::Result<()> {
        let path = Self::path(root.as_ref());
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

    pub fn record_fields(&mut self, fields: &BTreeMap<String, String>, policy: &IndexPolicy) {
        for (xpath, _) in fields {
            let kind = policy
                .fields
                .iter()
                .find(|f| f.name == *xpath)
                .map(|f| f.index.clone())
                .unwrap_or(IndexKind::None);

            self.fields.entry(xpath.clone()).or_insert(kind);
        }
    }

    pub fn get_fields(&self) -> &BTreeMap<String, IndexKind> {
        &self.fields
    }

    pub fn get_fields_mut(&mut self) -> &mut BTreeMap<String, IndexKind> {
        &mut self.fields
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}
