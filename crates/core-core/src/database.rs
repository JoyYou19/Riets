use std::{
    io,
    path::{Path, PathBuf},
};

use core_index::{
    analyzer::Analyzer,
    document::IndexPolicy,
    lsm::{
        LsmIndex,
        index_worker::{IndexingStats, ReindexStatus, ReindexingStats},
        worker::CompactionWorker,
    },
};
use core_protocol::errors::CorelamoError;
use core_query::{
    Query,
    planner::{QueryPlan, QueryPlanner},
    query_string_parser::parse_and_analyze,
};

use core_storage::{
    binary_store::BinaryDocumentStore,
    document_store::StoredDocument,
    search_database::{DocumentInput, IndexMode, InsertReport, SearchDatabase, SearchDocumentHit},
};
//logging
use core_logs::logger;
use slog::{Logger, error, info};

//logging
use crate::{
    command_reponse_definitions::SearchCommand, metrics::DatabaseMetrics, options::DatabaseOptions,
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
            return Err(CorelamoError::AlreadyExists(format!(
                "database at {} already exists",
                root.display()
            )));
        }
        std::fs::create_dir_all(&root)?;
        let log = logger::db_logger(&root, &name);
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
            return Err(CorelamoError::NotFound(format!(
                "no database found at {}",
                root.display()
            )));
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
            Some(CompactionWorker::start(
                db.index_sender(),
                self.options.runtime.compaction,
                self.options.compaction_interval,
            ))
        } else {
            None
        };

        self.db = Some(db);
        info!(self.log, "Database started");

        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), CorelamoError> {
        if let Some(worker) = self.compaction_worker.take() {
            worker.stop()?;
            info!(self.log, "Compaction worker stopped")
        }
        if let Some(db) = self.db.take() {
            db.shutdown()?;
            info!(self.log, "Database stopped")
        }
        Ok(())
    }

    pub fn restart(&mut self) -> Result<(), CorelamoError> {
        self.stop()?;
        info!(self.log, "database restarted");
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
        inputs: Vec<DocumentInput>,
    ) -> io::Result<InsertReport> {
        let started = std::time::Instant::now();
        let count = inputs.len();
        let batch_size = self.options.runtime.indexing_batch_size;
        let window_size = self.options.runtime.indexing_window_size;

        let result = (|| {
            let db = self.db_mut()?;

            let report = db.put_documents_parallel(inputs, batch_size, window_size)?;

            db.flush()?;
            Ok(report)
        })();

        let elapsed = started.elapsed();

        self.metrics.indexing_requests += 1;
        self.metrics.indexing_total_time += elapsed;

        if result.is_err() {
            self.metrics.indexing_errors += 1;
            error!(self.log, "put documents parallel failed";
                "documents" => count,
                "batch_size" =>batch_size,
                "elapsed_ms" => elapsed.as_millis(),
            );
        } else {
            info!(self.log, "put documents parallel";
                "documents" => count,
                "batch_size" =>batch_size,
                "elapsed_ms" => elapsed.as_millis(),
            );
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

    pub fn build_query(&self, input: &str) -> Result<Option<Query>, CorelamoError> {
        let db = self.db_ref()?;
        parse_and_analyze(input, db.get_analyzer())
    }

    //INFO: changed the function call to take a SearchCommand not just a string of query
    pub fn search(
        &mut self,
        command: &SearchCommand,
    ) -> Result<Vec<SearchDocumentHit>, CorelamoError> {
        let started = std::time::Instant::now();
        //TODO: make the docs 10 default configurable
        let limit = command.docs.unwrap_or(10);
        let offset = command.offset.unwrap_or(0);

        let result = (|| {
            let Some(query) = self.build_query(&command.query)? else {
                return Ok(Vec::new());
            };
            //INFO: you can see how the query got parsed to our structs
            let parsedq = format!("{:?}", query);
            info!(self.log, "query parsing result";
                "output" => parsedq,
                "input" => &command.query,

            );
            let plan = QueryPlanner::plan(query);
            self.search_plan(&plan, command.return_fields.as_ref(), offset, limit)
        })();

        let elapsed = started.elapsed();

        self.metrics.search_requests += 1;
        self.metrics.search_total_time += elapsed;

        match &result {
            Ok(hits) => {
                info!(
                    self.log,
                    "searched";
                    "query" => &command.query,
                    "offset" => offset,
                    "limit" => limit,
                    "returned" => hits.len(),
                    "elapsed_ms" => elapsed.as_millis(),
                );
            }
            Err(_) => {
                self.metrics.search_errors += 1;
                error!(
                    self.log,
                    "search failed";
                    "query" => &command.query,
                    "offset" => offset,
                    "limit" => limit,
                    "elapsed_ms" => elapsed.as_millis(),
                );
            }
        }

        return result.map_err(CorelamoError::from);
    }

    //INFO: old one now we have parse_query()
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
        self.db_mut()?
            .upsert_document(input, IndexMode::StoreAndIndex)
    }

    pub fn get_document(&mut self, external_id: &str) -> io::Result<Option<StoredDocument>> {
        self.db_mut()?.get_document(external_id)
    }

    fn db_mut(&mut self) -> io::Result<&mut SearchDatabase<BinaryDocumentStore>> {
        self.db
            .as_mut()
            .ok_or_else(|| io::Error::other("database is closed"))
    }

    fn db_ref(&self) -> io::Result<&SearchDatabase<BinaryDocumentStore>> {
        self.db
            .as_ref()
            .ok_or_else(|| io::Error::other("database is closed"))
    }

    pub fn search_plan(
        &mut self,
        plan: &QueryPlan,
        return_fields: Option<&IndexMap<String, bool>>,
        offset: usize,
        limit: usize,
    ) -> io::Result<Vec<SearchDocumentHit>> {
        self.db_mut()?
            .search_document_hits_plan(plan, return_fields, offset, limit)
    }

    // pub fn search_top_k(&mut self, query: &Query, k: usize) -> io::Result<Vec<SearchDocumentHit>> {
    //     self.db_mut()?
    //         .search_document_hits_all_fields_top_k(query, k)
    // }

    pub fn analyze_query_term(&self, term: &str) -> io::Result<Option<String>> {
        Ok(self.db_ref()?.analyze_query_term(term))
    }
    pub fn reindexing_receiver(&self) -> watch::Receiver<ReindexingStats> {
        self.reindexing_rx.clone()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    //FIX: wee need some way to filter the logs please like last:x or date:
    pub fn get_logs(&self) -> Result<String, CorelamoError> {
        let log_dir = self.root.join("logs");
        let name = self
            .root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let log_file = log_dir.join(format!("{name}.log"));

        if !log_file.exists() {
            return Ok(String::new());
        }

        std::fs::read_to_string(&log_file)
            .map_err(|e| CorelamoError::Internal(format!("failed to read logs: {e}")))
    }

    pub fn clear_logs(&mut self) -> Result<(), CorelamoError> {
        //delete logs
        let logs_dir = self.root.join("logs");
        if logs_dir.exists() {
            std::fs::remove_dir_all(&logs_dir)?;
            std::fs::create_dir_all(&logs_dir)?;
        }
        //fresh start
        let name = self
            .root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        self.log = logger::db_logger(&self.root, &name);

        info!(self.log, "logs cleared");
        Ok(())
    }

    pub fn clear(&mut self) -> io::Result<()> {
        if let Some(worker) = self.compaction_worker.take() {
            worker.stop()?;
        }

        if let Some(db) = self.db.as_mut() {
            db.index_worker().abort()?;
        }

        let index_root = self.root.join("index");
        std::fs::remove_dir_all(&index_root).ok();

        let store_path = self.root.join("documents.bin");
        std::fs::remove_file(&store_path).ok();

        //new slaves for database
        let analyzer = Analyzer::new();
        let index = LsmIndex::persistent(&index_root, self.options.runtime.flush_threshold)?;
        let store = BinaryDocumentStore::open(&store_path)?;

        let db = SearchDatabase::with_policy(store, index, analyzer, self.policy.clone());

        self.compaction_worker = if self.options.enable_background_compaction {
            Some(CompactionWorker::start(
                db.index_sender(),
                self.options.runtime.compaction,
                self.options.compaction_interval,
            ))
        } else {
            None
        };

        self.db = Some(db);

        Ok(())
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        if let Some(worker) = self.compaction_worker.take() {
            info!(self.log, "Compaction worker stopped");
            worker.stop()?;
        }
        if let Some(db) = self.db.take() {
            info!(self.log, "Database shut down");
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
        let mut indexing = db.index_worker().get_stats()?;
        if reindexing.documents_indexed > indexing.total_documents_indexed {
            indexing.total_documents_indexed = reindexing.documents_indexed;
        }

        Ok(DatabaseStats {
            document_count: db.document_count(),
            segment_count: db.segment_count()?,
            background_compaction_enabled: self.compaction_worker.is_some(),
            metrics: self.metrics.clone(),
            indexing,
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
        info!(self.log, "policy set"; "policy" => format!("{:?}", self.policy));
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        info!(self.log, "Flushing database");
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
        info!(self.log, "Policy reloaded");
        self.db_mut()?.set_policy(policy.clone())?;
        self.policy = policy;

        Ok(())
    }

    pub fn reindex(&mut self) -> io::Result<()> {
        info!(self.log, "reindex started");
        let _ = self.reindexing_tx.send(ReindexingStats {
            status: ReindexStatus::Reindexing,
            progress: 0,
            documents_indexed: 0,
            eta_seconds: None,
        });
        let watermark = {
            let mut max_id = 0u64;
            self.db_ref()?.for_each_document(
                &mut (|doc| {
                    max_id = max_id.max(doc.internal_id);
                    Ok(())
                }),
            )?;
            max_id
        };
        let temp_index_root = self.root.join("index.new");
        std::fs::remove_dir_all(&temp_index_root).ok();
        std::fs::create_dir_all(&temp_index_root)?;

        let analyzer = Analyzer::new();
        let new_index =
            LsmIndex::persistent(&temp_index_root, self.options.runtime.flush_threshold)?;
        let read_store = BinaryDocumentStore::open(self.root.join("documents.bin"))?;
        let mut staging_db = SearchDatabase::with_policy(
            read_store,
            new_index,
            analyzer.clone(),
            self.policy.clone(),
        );

        staging_db.reindex_existing_documents(
            self.options.runtime.indexing_batch_size,
            self.options.runtime.indexing_window_size,
            &self.reindexing_tx,
        )?;
        staging_db.shutdown_into_store()?;

        if let Some(worker) = self.compaction_worker.take() {
            worker.stop()?;
        }
        let old_db = self
            .db
            .take()
            .ok_or_else(|| io::Error::other("database is closed"))?;
        let store = old_db.shutdown_into_store()?;

        let index_root = self.root.join("index");
        std::fs::rename(&index_root, self.root.join("index.old"))?;
        std::fs::rename(&temp_index_root, &index_root)?;
        let index = LsmIndex::persistent(&index_root, self.options.runtime.flush_threshold)?;
        let mut db = SearchDatabase::with_policy(store, index, analyzer, self.policy.clone());

        // catch-up pass: index anything written after the watermark
        db.reindex_documents_after(watermark, self.options.runtime.indexing_batch_size)?;

        self.compaction_worker = if self.options.enable_background_compaction {
            Some(CompactionWorker::start(
                db.index_sender(),
                self.options.runtime.compaction,
                self.options.compaction_interval,
            ))
        } else {
            None
        };

        self.db = Some(db);
        std::fs::remove_dir_all(self.root.join("index.old")).ok();
        let _ = self.reindexing_tx.send(ReindexingStats {
            status: ReindexStatus::Complete,
            progress: 100,
            eta_seconds: None,
            documents_indexed: self
                .db
                .as_ref()
                .map(|d| d.document_count() as u64)
                .unwrap_or(0),
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
