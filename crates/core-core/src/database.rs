use std::{
    io,
    path::{Path, PathBuf},
};

use core_index::{
    analyzer::Analyzer,
    document::IndexPolicy,
    lsm::{LsmIndex, worker::CompactionWorker},
};
use core_query::Query;
use core_storage::{
    binary_store::BinaryDocumentStore,
    document_store::StoredDocument,
    search_database::{DocumentInput, SearchDatabase, SearchDocumentHit},
};

use crate::{metrics::DatabaseMetrics, options::DatabaseOptions};

// Currently the main entry point to the database
pub struct CorelamoDatabase {
    root: PathBuf,
    policy_path: PathBuf,
    policy: IndexPolicy,

    options: DatabaseOptions,
    db: Option<SearchDatabase<BinaryDocumentStore>>,
    compaction_worker: Option<CompactionWorker>,

    metrics: DatabaseMetrics,
}

impl CorelamoDatabase {
    pub fn open(root: impl AsRef<Path>, options: DatabaseOptions) -> io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;

        let index_root = root.join("index");
        let store_path = root.join("documents.bin");
        let policy_path = root.join("policy.toml");

        let policy = if policy_path.exists() {
            IndexPolicy::load(&policy_path)?
        } else {
            let policy = IndexPolicy::default_document();
            policy.save(&policy_path)?;
            policy
        };

        let analyzer = Analyzer::new();
        let index = LsmIndex::persistent(&index_root, options.runtime.flush_threshold)?;
        let store = BinaryDocumentStore::open(&store_path)?;

        let db = SearchDatabase::with_policy(store, index, analyzer, policy.clone());

        let compaction_worker = if options.enable_background_compaction {
            Some(CompactionWorker::start(
                db.index_sender(),
                options.runtime.compaction,
                options.compaction_interval,
            ))
        } else {
            None
        };

        Ok(Self {
            root,
            policy_path,
            policy,
            options,
            db: Some(db),
            compaction_worker,
            metrics: DatabaseMetrics::default(),
        })
    }

    pub fn put_documents_parallel(&mut self, inputs: Vec<DocumentInput>) -> io::Result<()> {
        let started = std::time::Instant::now();
        let count = inputs.len();
        let batch_size = self.options.runtime.indexing_batch_size;

        let result = (|| {
            let db = self.db_mut()?;
            db.put_documents_parallel(inputs, batch_size)?;
            db.flush()
        })();

        let elapsed = started.elapsed();

        self.metrics.indexing_requests += 1;
        self.metrics.indexing_total_time += elapsed;

        if result.is_err() {
            self.metrics.indexing_errors += 1;
        }

        tracing::info!(
            documents = count,
            batch_size = batch_size,
            elapsed_ms = elapsed.as_millis(),
            ok = result.is_ok(),
            "put documents parallel"
        );

        result
    }

    pub fn search(&mut self, query: &str, k: usize) -> io::Result<Vec<SearchDocumentHit>> {
        let started = std::time::Instant::now();

        let result = (|| {
            let Some(query) = self.build_query(query)? else {
                return Ok(Vec::new());
            };

            self.search_top_k(&query, k)
        })();

        let elapsed = started.elapsed();

        self.metrics.search_requests += 1;
        self.metrics.search_total_time += elapsed;

        if result.is_err() {
            self.metrics.search_errors += 1;
        }

        tracing::info!(
            query = query,
            k = k,
            elapsed_ms = elapsed.as_millis(),
            ok = result.is_ok(),
            "search request"
        );

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

    //HACK: not 100% sure if this is corret but we need RETRIEVE sorry Valč
    pub fn get_document(&mut self, external_id: &str) -> io::Result<Option<StoredDocument>> {
        self.db_mut()?.get_document(external_id)
    }

    fn db_mut(&mut self) -> io::Result<&mut SearchDatabase<BinaryDocumentStore>> {
        self.db
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "database is closed"))
    }

    fn db_ref(&self) -> io::Result<&SearchDatabase<BinaryDocumentStore>> {
        self.db
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "database is closed"))
    }

    pub fn search_top_k(&mut self, query: &Query, k: usize) -> io::Result<Vec<SearchDocumentHit>> {
        self.db_mut()?
            .search_document_hits_all_fields_top_k(query, k)
    }

    pub fn analyze_query_term(&self, term: &str) -> io::Result<Option<String>> {
        Ok(self.db_ref()?.analyze_query_term(term))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn shutdown(mut self) -> io::Result<()> {
        if let Some(worker) = self.compaction_worker.take() {
            worker.stop()?;
        }

        if let Some(db) = self.db.take() {
            let _index = db.shutdown()?;
        }

        Ok(())
    }

    pub fn stats(&self) -> io::Result<DatabaseStats> {
        let db = self.db_ref()?;

        Ok(DatabaseStats {
            document_count: db.document_count(),
            segment_count: db.segment_count()?,
            background_compaction_enabled: self.compaction_worker.is_some(),
            metrics: self.metrics.clone(),
        })
    }

    pub fn set_policy(&mut self, policy: IndexPolicy) -> io::Result<()> {
        policy.validate()?;

        self.db_mut()?.set_policy(policy.clone())?;
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
        self.policy.save(&self.policy_path)
    }

    pub fn reload_policy(&mut self) -> io::Result<()> {
        let policy = IndexPolicy::load(&self.policy_path)?;

        self.db_mut()?.set_policy(policy.clone())?;
        self.policy = policy;

        Ok(())
    }

    pub fn reindex(&mut self) -> io::Result<()> {
        if let Some(worker) = self.compaction_worker.take() {
            worker.stop()?;
        }

        let db = self
            .db
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "database is closed"))?;

        let store = db.shutdown_into_store()?;

        let index_root = self.root.join("index");
        std::fs::remove_dir_all(&index_root).ok();

        let analyzer = Analyzer::new();
        let index = LsmIndex::persistent(&index_root, self.options.runtime.flush_threshold)?;

        let mut db = SearchDatabase::with_policy(store, index, analyzer, self.policy.clone());

        db.reindex_existing_documents(self.options.runtime.indexing_batch_size)?;

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseStats {
    pub document_count: usize,
    pub segment_count: usize,
    pub background_compaction_enabled: bool,
    pub metrics: DatabaseMetrics,
}
