use core_index::analyzer::Analyzer;
use core_protocol::command_reponse_definitions::{LookupCommand, LookupResponse, SearchCommand};
use core_storage::document_store::StoredDocument;
use crossbeam_channel::bounded;
use parking_lot::RwLock;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::ShardDb;
use crate::shard_worker::{self, ShardCmd, ShardHandle};
use crate::{DatabaseOptions, shard_for};
use core_index::document::IndexPolicy;
use core_index::types::ShardId;
use core_protocol::errors::CorelamoError;
use core_query::query_string_parser::parse_and_analyze;
use core_storage::search_database::InsertReport;
use core_storage::search_database::{DocumentInput, SearchDocumentHit};

pub struct ShardManager {
    shards: Vec<ShardHandle>,
    joins: Vec<JoinHandle<()>>, //JoinHandle atgriez ka thread uztaisits
    root: PathBuf,
    policy: RwLock<IndexPolicy>,
    options: RwLock<DatabaseOptions>,
    analyzer: Analyzer,
}

impl ShardManager {
    const CONFIG_PATH_NAME: &'static str = "config.toml";
    const POLICY_PATH_NAME: &'static str = "policy.toml";
    const DEFAULT_QUEUE_DEPTH: usize = 256;

    fn config_path(root: &Path) -> PathBuf {
        root.join(Self::CONFIG_PATH_NAME)
    }

    fn policy_path(root: &Path) -> PathBuf {
        root.join(Self::POLICY_PATH_NAME)
    }
    pub fn all_alive(&self) -> bool {
        self.shards.iter().all(|h| h.is_alive())
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
        let mut joins = Vec::new();
        for shard_id in 0..num_shards {
            let shard_root = shards_dir.join(format!("shard-{}", shard_id));
            let db = ShardDb::create_shard(
                &shard_root,
                ShardId::from(shard_id),
                options.clone(),
                policy.clone(),
            )?;
            let (handle, join) = shard_worker::spawn(db, Self::DEFAULT_QUEUE_DEPTH)?;
            shards.push(handle);
            joins.push(join);
        }

        Ok(Self {
            shards,
            joins,
            root,
            policy: RwLock::new(policy),
            options: RwLock::new(options),
            analyzer: Analyzer::new(),
        })
    }

    pub fn create_and_start(
        root: PathBuf,
        num_shards: u16,
        options: DatabaseOptions,
    ) -> Result<Self, CorelamoError> {
        Self::create(root, num_shards, options)
    }

    pub fn clear_all(&self) -> Result<(), CorelamoError> {
        let pending: Vec<_> = self
            .shards
            .iter()
            .map(|h| {
                let (rtx, rrx) = bounded(1);
                let _ = h.send_raw(ShardCmd::Clear { resp: rtx });
                rrx
            })
            .collect();

        let mut first_err = None;
        for rx in pending {
            match rx.recv() {
                Ok(Err(e)) if first_err.is_none() => first_err = Some(e),
                Err(_) if first_err.is_none() => {
                    first_err = Some(CorelamoError::Internal("shard died during clear".into()))
                }
                _ => {}
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
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
        if shard_paths.len() != (expected_num_shards as usize) {
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
        let mut joins = Vec::new();
        for shard_path in shard_paths {
            let db = ShardDb::load(&shard_path, &policy, &options)?;
            let (handle, join) = shard_worker::spawn(db, Self::DEFAULT_QUEUE_DEPTH)?;
            shards.push(handle);
            joins.push(join);
        }

        Ok(Self {
            shards,
            joins,
            root,
            policy: RwLock::new(policy),
            options: RwLock::new(options),
            analyzer: Analyzer::new(),
        })
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    pub fn shutdown(self) -> Result<(), CorelamoError> {
        let pending: Vec<_> = self
            .shards
            .iter()
            .map(|h| {
                let (rtx, rrx) = bounded(1);
                let _ = h.send_raw(ShardCmd::Shutdown { resp: rtx });
                rrx
            })
            .collect();

        let mut first_err = None;
        for rx in pending {
            match rx.recv() {
                Ok(Err(e)) if first_err.is_none() => {
                    first_err = Some(e);
                }
                _ => {}
            }
        }
        for j in self.joins {
            let _ = j.join();
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    pub fn policy(&self) -> IndexPolicy {
        self.policy.read().clone()
    }

    pub fn options(&self) -> DatabaseOptions {
        self.options.read().clone()
    }

    pub fn set_policy_all(&self, policy: IndexPolicy) -> Result<(), CorelamoError> {
        policy.validate()?;

        let pending: Vec<_> = self
            .shards
            .iter()
            .map(|h| {
                let (rtx, rrx) = bounded(1);
                let _ = h.send_raw(ShardCmd::SetPolicy {
                    policy: policy.clone(),
                    resp: rtx,
                });
                rrx
            })
            .collect();
        for rx in pending {
            rx.recv()
                .map_err(|_| CorelamoError::Internal("shard died".into()))??;
        }

        let mut guard = self.policy.write();
        guard.save(&Self::policy_path(&self.root))?;
        *guard = policy;
        Ok(())
    }

    pub fn set_options_all(&self, options: DatabaseOptions) -> Result<(), CorelamoError> {
        let pending: Vec<_> = self
            .shards
            .iter()
            .map(|h| {
                let (rtx, rrx) = bounded(1);
                let _ = h.send_raw(ShardCmd::SetConfig {
                    options: options.clone(),
                    resp: rtx,
                });
                rrx
            })
            .collect();
        for rx in pending {
            rx.recv()
                .map_err(|_| CorelamoError::Internal("shard died".into()))??;
        }

        options.save_to_file(&Self::config_path(&self.root))?;
        *self.options.write() = options;
        Ok(())
    }
    //write operations

    //paligfunkcija no viber
    fn shard_index_for(&self, external_id: &str) -> usize {
        shard_for(external_id, self.shards.len() as u16) as usize
    }

    fn group_by_shard<'a>(&self, ids: &'a [String]) -> HashMap<usize, Vec<&'a String>> {
        let mut by_shard: HashMap<usize, Vec<&'a String>> = HashMap::new();
        for id in ids {
            by_shard
                .entry(self.shard_index_for(id))
                .or_default()
                .push(id);
        }
        by_shard
    }

    pub fn insert(&self, inputs: Vec<DocumentInput>) -> Result<InsertReport, CorelamoError> {
        let mut by_shard: HashMap<usize, Vec<DocumentInput>> = HashMap::new();
        for input in inputs {
            by_shard
                .entry(self.shard_index_for(&input.external_id))
                .or_default()
                .push(input);
        }

        let mut pending = Vec::with_capacity(by_shard.len());
        for (idx, batch) in by_shard {
            let (rtx, rrx) = bounded(1);
            self.shards[idx]
                .send_raw(ShardCmd::Insert {
                    inputs: batch,
                    resp: rtx,
                })
                .map_err(|(e, _)| e)?;
            pending.push(rrx);
        }

        let mut report = InsertReport {
            inserted: 0,
            failures: Vec::new(),
        };
        for rx in pending {
            let r = rx
                .recv()
                .map_err(|_| CorelamoError::Internal("shard died during insert".into()))??;
            report.inserted += r.inserted;
            report.failures.extend(r.failures);
        }
        Ok(report)
    }

    pub fn lookup(&self, command: &LookupCommand) -> Result<LookupResponse, CorelamoError> {
        let by_shard = self.group_by_shard(&command.ids);
        let mut pending = Vec::with_capacity(by_shard.len());

        for (idx, shard_ids) in by_shard {
            let (rtx, rrx) = bounded(1);
            let shard_command = LookupCommand {
                ids: shard_ids.iter().map(|s| s.to_string()).collect(),
                return_fields: command.return_fields.clone(),
            };
            self.shards[idx]
                .send_raw(ShardCmd::Lookup {
                    command: shard_command,
                    resp: rtx,
                })
                .map_err(|(e, _)| e)?;
            pending.push(rrx);
        }

        let mut all_docs = Vec::new();
        let mut all_not_found = Vec::new();
        for rx in pending {
            let response = rx
                .recv()
                .map_err(|_| CorelamoError::Internal("shard died during lookup".into()))??;
            all_docs.extend(response.docs);
            all_not_found.extend(response.not_found);
        }

        Ok(LookupResponse {
            docs: all_docs,
            not_found: all_not_found,
        })
    }

    pub fn search(&self, command: &SearchCommand) -> Result<Vec<SearchDocumentHit>, CorelamoError> {
        let limit = command.docs.unwrap_or(10);
        let offset = command.offset.unwrap_or(0);
        let fetch = offset.saturating_add(limit);

        let Some(query) = parse_and_analyze(&command.query, &self.analyzer)? else {
            return Ok(Vec::new());
        };
        let query = Arc::new(query);

        let mut pending = Vec::with_capacity(self.shards.len());
        for h in &self.shards {
            let (rtx, rrx) = bounded(1);
            if h.send_raw(ShardCmd::Search {
                query: query.clone(),
                k: fetch,
                resp: rtx,
            })
            .is_ok()
            {
                pending.push(rrx);
            }
        }

        let mut all_hits: Vec<SearchDocumentHit> = Vec::new();
        for rx in pending {
            if let Ok(Ok(hits)) = rx.recv() {
                all_hits.extend(hits);
            }
        }

        all_hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.internal_id.cmp(&b.internal_id))
        });
        Ok(all_hits.into_iter().skip(offset).take(limit).collect())
    }
    pub fn retrieve(
        &self,
        ids: Vec<String>,
    ) -> Result<Vec<(String, Option<StoredDocument>)>, CorelamoError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // remember where each id was asked for; the fan-out returns them shard-grouped
        let mut position: HashMap<String, usize> = HashMap::new();
        for (i, id) in ids.iter().enumerate() {
            position.entry(id.clone()).or_insert(i); //catins teica noder
        }

        let mut by_shard: HashMap<usize, Vec<String>> = HashMap::new();
        for id in ids {
            by_shard
                .entry(self.shard_index_for(&id))
                .or_default()
                .push(id);
        }
        let mut pending = Vec::with_capacity(by_shard.len());
        for (idx, batch) in by_shard {
            let (rtx, rrx) = bounded(1);
            self.shards[idx]
                .send_raw(ShardCmd::Retrieve {
                    ids: batch,
                    resp: rtx,
                })
                .map_err(|(e, _)| e)?;
            pending.push(rrx);
        }

        let mut out: Vec<(String, Option<StoredDocument>)> = Vec::with_capacity(position.len());
        for rx in pending {
            let batch = rx
                .recv()
                .map_err(|_| CorelamoError::Internal("shard died during retrieve".into()))??;
            out.extend(batch);
        }

        out.sort_by_key(|(id, _)| position.get(id).copied().unwrap_or(usize::MAX));
        Ok(out)
    }
    // TODO: delete, upsert, lookup, etc.
}
