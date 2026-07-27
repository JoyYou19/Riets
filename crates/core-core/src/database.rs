use std::{ io, path::{ Path, PathBuf } };

use core_index::{
    analyzer::Analyzer,
    document::IndexPolicy,
    lsm::{
        LsmIndex,
        index_worker::{ IndexingStats, ReindexStatus, ReindexingStats },
        worker::CompactionWorker,
    },
};
use core_protocol::errors::CorelamoError;
use core_query::{ Query, planner::{ QueryPlan, QueryPlanner } };

use core_storage::{
    binary_store::BinaryDocumentStore,
    document_store::StoredDocument,
    search_database::{ DocumentInput, IndexMode, InsertReport, SearchDatabase, SearchDocumentHit },
};
//logging
use core_logs::logger;
use slog::{ Logger, info, warn, error };
use slog_async::AsyncGuard;
//logging
use crate::{
    command_reponse_definitions::SearchCommand,
    metrics::DatabaseMetrics,
    options::DatabaseOptions,
};
use indexmap::IndexMap;
use tokio::sync::watch;

// Currently the main entry point to the database
pub struct CorelamoDatabase {
    root: PathBuf,
    policy_path: PathBuf,
    policy: IndexPolicy,
    options: DatabaseOptions,
    db: Option<SearchDatabase<BinaryDocumentStore>>,
    compaction_worker: Option<CompactionWorker>,
    metrics: DatabaseMetrics,
    reindexing_tx: watch::Sender<ReindexingStats>,
    reindexing_rx: watch::Receiver<ReindexingStats>,
    log: Logger,

}

impl CorelamoDatabase {
    fn config_full_path_from(root: &Path) -> std::path::PathBuf {
        root.join("config.toml")
    }
    //INFO: just creates everything for the database, doesnt start it
    pub fn create(root: impl AsRef<Path>, options: DatabaseOptions) -> Result<Self, CorelamoError> {
        let root = root.as_ref().to_path_buf();
        let name = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown viss slikiti")
            .to_string();
        
        if root.exists() {
            return Err(
                CorelamoError::AlreadyExists(
                    format!("database at {} already exists", root.display())
                )
            );
        }
        std::fs::create_dir_all(&root)?;
        let log = logger::db_logger(&root, &name);
        slog::info!(log, "immediate test line"; "test" => true);
        std::thread::sleep(std::time::Duration::from_secs(1));
        //reindexing
        let (reindexing_tx, reindexing_rx) = watch::channel(ReindexingStats::default());
        let policy_path = root.join("policy.toml");
        let policy = IndexPolicy::default_document();
        policy.save(&policy_path)?;
        options.save_to_file(Self::config_full_path_from(&root))?;

        let store_path = root.join("documents.bin");
        BinaryDocumentStore::open(&store_path)?;

        Ok(Self {
            root,
            policy_path,
            policy,
            options,
            db: None,
            compaction_worker: None,
            metrics: DatabaseMetrics::default(),
            reindexing_tx,
            reindexing_rx,
            log,
        })
    }

    //acknowledge the database on startup
    pub fn load(root: impl AsRef<Path>) -> Result<Self, CorelamoError> {
        let root = root.as_ref().to_path_buf();
        let policy_path = root.join("policy.toml");
        let name = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        

        if !policy_path.exists() {
            return Err(CorelamoError::NotFound(format!("no database found at {}", root.display())));
        }

        //reindexing
        let (reindexing_tx, reindexing_rx) = watch::channel(ReindexingStats::default());
        let policy = IndexPolicy::load(&policy_path)?;
        let options = DatabaseOptions::load_or_default(Self::config_full_path_from(&root));
         let log = logger::db_logger(&root, &name);
        Ok(Self {
            root,
            policy_path,
            policy,
            options,
            db: None,
            compaction_worker: None,
            metrics: DatabaseMetrics::default(),
            reindexing_tx,
            reindexing_rx,
            log,
        
            
        })
    }

    pub fn start(&mut self) -> Result<(), CorelamoError> {
        if self.db.is_some() {
            return Ok(());
        }
        info!(self.log, "database started");
        let index_root = self.root.join("index");
        let store_path = self.root.join("documents.bin");

        let analyzer = Analyzer::new();
        let index = LsmIndex::persistent(&index_root, self.options.runtime.flush_threshold)?;
        let store = BinaryDocumentStore::open(&store_path)?;

        let db = SearchDatabase::with_policy(store, index, analyzer, self.policy.clone());

        self.compaction_worker = if self.options.enable_background_compaction {
            Some(
                CompactionWorker::start(
                    db.index_sender(),
                    self.options.runtime.compaction,
                    self.options.compaction_interval
                )
            )
        } else {
            None
        };

        self.db = Some(db);
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), CorelamoError> {
        if let Some(worker) = self.compaction_worker.take() {
            worker.stop()?;
            info!(self.log,"Compaction worker stopped")
        }
        if let Some(db) = self.db.take() {
            db.shutdown()?;
            info!(self.log, "Database stopped")
        }
        Ok(())
    }

    pub fn restart(&mut self) -> Result<(), CorelamoError> {
        self.stop()?;
        info!(self.log,"database restarted");
        self.start()
        
    }

    pub fn set_options(&mut self, options: DatabaseOptions) -> Result<(), CorelamoError> {
        options.save_to_file(Self::config_full_path_from(&self.root))?;
        self.options = options;
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.db.is_some()
    }

    pub fn options(&self) -> &DatabaseOptions {
        &self.options
    }

    pub fn put_documents_parallel(
        &mut self,
        inputs: Vec<DocumentInput>
    ) -> io::Result<InsertReport> {
        let started = std::time::Instant::now();
        let count = inputs.len();
        let batch_size = self.options.runtime.indexing_batch_size;

        let result = (|| {
            let db = self.db_mut()?;
            let report = db.put_documents_parallel(inputs, batch_size)?;
            db.flush()?;
            Ok(report)
        })();

        let elapsed = started.elapsed();

        self.metrics.indexing_requests += 1;
        self.metrics.indexing_total_time += elapsed;

        if result.is_err() {
            self.metrics.indexing_errors += 1;
        }

   
            if result.is_err() {
                error!(self.log, "indexing failed";
                    "documents" => count,
                    "batch_size" => batch_size,
                    "elapsed_ms" => elapsed.as_millis(),
                );
            } else {
                info!(self.log, "indexed batch";
                    "documents" => count,
                    "batch_size" => batch_size,
                    "elapsed_ms" => elapsed.as_millis(),
                );
            }
        

        result
    }

    //INFO: changed the function call to take a SearchCommand not just a string of query
    pub fn search(&mut self, command: &SearchCommand) -> io::Result<Vec<SearchDocumentHit>> {
        let started = std::time::Instant::now();
        //TODO: make the docs 10 default configurable
        let docs = command.docs.unwrap_or(10);

        let result = (|| {
            let Some(query) = self.build_query(&command.query)? else {
                return Ok(Vec::new());
            };
            let plan = QueryPlanner::plan(query);
            self.search_plan_top_k(&plan, command.return_fields.as_ref(), docs)
        })();

        let elapsed = started.elapsed();

        self.metrics.search_requests += 1;
        self.metrics.search_total_time += elapsed;

        if result.is_err() {
            self.metrics.search_errors += 1;
        }

        if result.is_err() {
            self.metrics.search_errors += 1;
            error!(self.log, "search failed";
        "query" => &command.query,
        "k" => docs,
        "elapsed_ms" => elapsed.as_millis(),
    );
        } else {
            info!(self.log, "search";
        "query" => &command.query,
        "k" => docs,
        "elapsed_ms" => elapsed.as_millis(),
    );
        }

        result
    }

    fn build_query(&self, input: &str) -> io::Result<Option<Query>> {
        let terms: Vec<String> = input
            .split_whitespace()
            .map(|term| term.to_string())
            .collect();

        Ok(match terms.len() {
            0 => None,
            1 => Some(Query::Term(terms[0].clone())),
            _ => Some(Query::And(terms.into_iter().map(Query::Term).collect())),
        })
    }

    // TODO: Might be broken.
    // fn build_query(&self, input: &str) -> io::Result<Option<Query>> {
    //     let db = self.db_ref()?;
    //
    //     let terms: Vec<String> = input
    //         .split_whitespace()
    //         .filter_map(|term| db.analyze_query_term(term))
    //         .collect();
    //
    //     Ok(match terms.len() {
    //         0 => None,
    //         1 => Some(Query::Term(terms[0].clone())),
    //         _ => Some(Query::And(terms.into_iter().map(Query::Term).collect())),
    //     })
    // }
    //

    pub fn delete_document(&mut self, external_id: &str) -> io::Result<()> {
        self.db_mut()?.delete_document(external_id)
    }

    pub fn upsert_document(&mut self, input: DocumentInput) -> io::Result<()> {
        self.db_mut()?.upsert_document(input, IndexMode::StoreAndIndex)
    }

    pub fn get_document(&mut self, external_id: &str) -> io::Result<Option<StoredDocument>> {
        self.db_mut()?.get_document(external_id)
    }

    fn db_mut(&mut self) -> io::Result<&mut SearchDatabase<BinaryDocumentStore>> {
        self.db.as_mut().ok_or_else(|| io::Error::other("database is closed"))
    }

    fn db_ref(&self) -> io::Result<&SearchDatabase<BinaryDocumentStore>> {
        self.db.as_ref().ok_or_else(|| io::Error::other("database is closed"))
    }

    pub fn search_plan_top_k(
        &mut self,
        plan: &QueryPlan,
        return_fields: Option<&IndexMap<String, bool>>,
        k: usize
    ) -> io::Result<Vec<SearchDocumentHit>> {
        self.db_mut()?.search_document_hits_plan_top_k(plan, return_fields, k)
    }

    pub fn search_top_k(&mut self, query: &Query, k: usize) -> io::Result<Vec<SearchDocumentHit>> {
        self.db_mut()?.search_document_hits_all_fields_top_k(query, k)
    }

    pub fn analyze_query_term(&self, term: &str) -> io::Result<Option<String>> {
        Ok(self.db_ref()?.analyze_query_term(term))
    }
    pub fn reindexing_receiver(&self) -> watch::Receiver<ReindexingStats> {
        self.reindexing_rx.clone()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        if let Some(worker) = self.compaction_worker.take() {
            worker.stop()?;
        }
        if let Some(db) = self.db.take() {
            let _index = db.shutdown()?;
        }
        Ok(())
    }
    // ja notiek reindex stats nevar dabut tapec fallback uz reindexing tikai
    pub fn stats(&self) -> io::Result<DatabaseStats> {
        let reindexing = self.reindexing_rx.borrow().clone();
        let db = match self.db_ref() {
            Ok(db) => db,
            Err(e) => {
                if reindexing.status == ReindexStatus::Reindexing {
                    return Ok(DatabaseStats {
                        document_count: 0,
                        segment_count: 0,
                        background_compaction_enabled: false,
                        metrics: self.metrics.clone(),
                        indexing: IndexingStats::default(),
                        reindexing,
                    });
                }
                return Err(e);
            }
        };

        Ok(DatabaseStats {
            document_count: db.document_count(),
            segment_count: db.segment_count()?,
            background_compaction_enabled: self.compaction_worker.is_some(),
            metrics: self.metrics.clone(),
            indexing: db.index_worker().get_stats()?,
            reindexing,
        })
    }

    //SMART SHIIT: if database running update the policy if not just validate->update file
    pub fn set_policy(&mut self, policy: IndexPolicy) -> io::Result<()> {
        policy.validate()?;

        if self.db.is_some() {
            self.db_mut()?.set_policy(policy.clone())?;
        }

        self.policy = policy;
        self.save_policy()?;

        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.db_mut()?.flush()
    }

    pub fn policy(&self) -> &IndexPolicy {
        &self.policy
    }

    pub fn policy_path(&self) -> &Path {
        &self.policy_path
    }

    pub fn save_policy(&self) -> io::Result<()> {
        self.policy.save(&self.policy_path)?;
        info!(self.log, "policy saved"; "policy" => format!("{:?}", self.policy));
        Ok(())
    }

    pub fn reload_policy(&mut self) -> io::Result<()> {
        let policy = IndexPolicy::load(&self.policy_path)?;

        self.db_mut()?.set_policy(policy.clone())?;
        self.policy = policy;

        Ok(())
    }

    pub fn reindex(&mut self) -> io::Result<()> {
        info!(self.log, "reindex started");
        let _ = self.reindexing_tx.send(ReindexingStats {
            status: ReindexStatus::Reindexing,
            progress: 0,
            eta_seconds: None,
        });
        let watermark = {
            let mut max_id = 0u64;
            self.db_ref()?.for_each_document(
                &mut (|doc| {
                    max_id = max_id.max(doc.internal_id);
                    Ok(())
                })
            )?;
            max_id
        };
        let temp_index_root = self.root.join("index.new");
        std::fs::remove_dir_all(&temp_index_root).ok();
        std::fs::create_dir_all(&temp_index_root)?;

        let analyzer = Analyzer::new();
        let new_index = LsmIndex::persistent(
            &temp_index_root,
            self.options.runtime.flush_threshold
        )?;
        let read_store = BinaryDocumentStore::open(self.root.join("documents.bin"))?;
        let mut staging_db = SearchDatabase::with_policy(
            read_store,
            new_index,
            analyzer.clone(),
            self.policy.clone()
        );
        staging_db.reindex_existing_documents(
            self.options.runtime.indexing_batch_size,
            &self.reindexing_tx
        )?;
        staging_db.shutdown_into_store()?;

        if let Some(worker) = self.compaction_worker.take() {
            worker.stop()?;
        }
        let old_db = self.db.take().ok_or_else(|| io::Error::other("database is closed"))?;
        let store = old_db.shutdown_into_store()?;

        let index_root = self.root.join("index");
        std::fs::rename(&index_root, self.root.join("index.old"))?;
        std::fs::rename(&temp_index_root, &index_root)?;
        let index = LsmIndex::persistent(&index_root, self.options.runtime.flush_threshold)?;
        let mut db = SearchDatabase::with_policy(store, index, analyzer, self.policy.clone());

        // catch-up pass: index anything written after the watermark
        db.reindex_documents_after(watermark, self.options.runtime.indexing_batch_size)?;

        self.compaction_worker = if self.options.enable_background_compaction {
            Some(
                CompactionWorker::start(
                    db.index_sender(),
                    self.options.runtime.compaction,
                    self.options.compaction_interval
                )
            )
        } else {
            None
        };

        self.db = Some(db);
        std::fs::remove_dir_all(self.root.join("index.old")).ok();
        let _ = self.reindexing_tx.send(ReindexingStats {
            status: ReindexStatus::Complete,
            progress: 100,
            eta_seconds: None,
        });
        info!(self.log, "reindex complete");
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DatabaseStats {
    pub document_count: usize,
    pub segment_count: usize,
    pub background_compaction_enabled: bool,
    pub metrics: DatabaseMetrics,
    pub indexing: IndexingStats,
    pub reindexing: ReindexingStats,
}
