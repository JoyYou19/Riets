use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::ShardDb;
use crate::{DatabaseOptions, shard_for};
use core_index::document::IndexPolicy;
use core_protocol::errors::CorelamoError;
use core_storage::search_database::{DocumentInput, InsertReport, SearchDocumentHit};

pub struct ShardManager {
    shards: Vec<ShardDb>,
    root: PathBuf,
    policy: IndexPolicy,
    options: DatabaseOptions,
}

impl ShardManager {
    const CONFIG_PATH_NAME: &'static str = "config.toml";
    const POLICY_PATH_NAME: &'static str = "policy.toml";

    fn config_path(root: &Path) -> PathBuf {
        root.join(Self::CONFIG_PATH_NAME)
    }

    fn policy_path(root: &Path) -> PathBuf {
        root.join(Self::POLICY_PATH_NAME)
    }

    pub fn create(
        root: PathBuf,
        num_shards: u16,
        options: DatabaseOptions,
    ) -> Result<Self, CorelamoError> {
        if num_shards == 0 {
            return Err(CorelamoError::InvalidData(
                "num_shards must be > 0".to_string(),
            ));
        }

        let shards_dir = root.join("shards");
        std::fs::create_dir_all(&shards_dir)?;

        //single policy and config
        let policy = IndexPolicy::default_document();
        policy.save(&Self::policy_path(&root))?;
        options.save_to_file(&Self::config_path(&root))?;

        let mut shards = Vec::new();
        for shard_id in 0..num_shards {
            let shard_root = shards_dir.join(format!("shard-{}", shard_id));

            //create shard 3000
            let db = ShardDb::create_shard(&shard_root, shard_id, options.clone(), policy.clone())?;
            shards.push(db);
        }

        Ok(Self {
            shards,
            root,
            policy,
            options,
        })
    }

    pub fn load(root: PathBuf, expected_num_shards: u16) -> Result<Self, CorelamoError> {
        let shards_dir = root.join("shards");

        if !shards_dir.exists() {
            return Err(CorelamoError::NotFound(format!(
                "shards directory not found at {}",
                shards_dir.display()
            )));
        }

        let mut shard_paths = Vec::new();
        for entry in std::fs::read_dir(&shards_dir)? {
            let entry = entry?;
            if entry.path().is_dir() {
                shard_paths.push(entry.path());
            }
        }
        shard_paths.sort();

        //INFO: safety check in case this might not be entirely necessary
        if shard_paths.len() != expected_num_shards as usize {
            return Err(CorelamoError::InvalidData(format!(
                "expected {} shards but found {} on disk",
                expected_num_shards,
                shard_paths.len()
            )));
        }

        let policy_path = Self::policy_path(&root);
        let config_path = Self::config_path(&root);

        if !policy_path.exists() {
            return Err(CorelamoError::NotFound(format!(
                "policy not found at {}",
                policy_path.display()
            )));
        }

        //FIX: we probably need a load_or_default for policy too
        let policy = IndexPolicy::load(&policy_path)?;
        let options = DatabaseOptions::load_or_default(&config_path);

        let mut shards = Vec::new();
        for shard_path in shard_paths {
            let db = ShardDb::load(&shard_path, &policy, &options)?;
            shards.push(db);
        }

        Ok(Self {
            shards,
            root,
            policy,
            options,
        })
    }

    pub fn start_all(&mut self) -> Result<(), CorelamoError> {
        for shard in &mut self.shards {
            shard.start()?;
        }
        Ok(())
    }

    pub fn stop_all(&mut self) -> Result<(), CorelamoError> {
        for shard in &mut self.shards {
            shard.stop()?;
        }
        Ok(())
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    pub fn policy(&self) -> &IndexPolicy {
        &self.policy
    }

    pub fn options(&self) -> &DatabaseOptions {
        &self.options
    }

    pub fn set_policy_all(&mut self, policy: IndexPolicy) -> Result<(), CorelamoError> {
        policy.validate()?;

        for shard in &mut self.shards {
            shard.set_policy(policy.clone())?;
        }

        self.policy = policy;
        self.policy.save(&Self::policy_path(&self.root))?;

        Ok(())
    }

    pub fn set_options_all(&mut self, options: DatabaseOptions) -> Result<(), CorelamoError> {
        options.save_to_file(&Self::config_path(&self.root))?;
        self.options = options;
        Ok(())
    }

    pub fn insert(&mut self, inputs: Vec<DocumentInput>) -> Result<InsertReport, CorelamoError> {
        // Group by shard
        let mut by_shard: HashMap<u16, Vec<DocumentInput>> = HashMap::new();
        for input in inputs {
            let shard_id = shard_for(&input.external_id, self.shards.len() as u16);
            by_shard
                .entry(shard_id)
                .or_insert_with(Vec::new)
                .push(input);
        }

        // Insert each shard's batch (each shard locks itself)
        let mut total_inserted = 0;
        let mut total_failures = Vec::new();

        for (shard_id, shard_inputs) in by_shard {
            let report = self.shards[shard_id as usize].insert(shard_inputs)?;
            total_inserted += report.inserted;
            total_failures.extend(report.failures);
        }

        Ok(InsertReport {
            inserted: total_inserted,
            failures: total_failures,
        })
    }

    // pub fn search(
    //     &self,
    //     query: &core_query::Query,
    //     xpath: u32,
    //     k: usize,
    // ) -> Result<Vec<SearchDocumentHit>, CorelamoError> {
    //     let mut all_hits = Vec::new();
    //
    //     for shard in &self.shards {
    //         let hits = shard.search(query, xpath, k)?;
    //         all_hits.extend(hits);
    //     }
    //
    //     all_hits.sort_by(|a, b| {
    //         b.score
    //             .partial_cmp(&a.score)
    //             .unwrap_or(Ordering::Equal)
    //             .then_with(|| a.internal_id.cmp(&b.internal_id))
    //     });
    //     all_hits.truncate(k);
    //
    //     Ok(all_hits)
    // }

    // TODO: delete, upsert, lookup, etc.
}
