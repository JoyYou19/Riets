use std::path::PathBuf;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, bounded};

use core_index::analyzer::Analyzer;
use core_index::document::IndexPolicy;
use core_index::lsm::LsmIndex;
use core_index::lsm::index_worker::ReindexProgress;
use core_index::types::ShardId;
use core_protocol::errors::CorelamoError;
use core_storage::binary_store::BinaryDocumentStore;
use core_storage::search_database::SearchDatabase;
use core_timing::timed;

use crate::metrics::DbStats;
use crate::{DatabaseOptions, shard_worker::ShardCmd};

pub struct ReindexParams {
    pub shard_id: ShardId,
    pub shard_root: PathBuf,
    pub policy: IndexPolicy,
    pub options: DatabaseOptions,
    pub wal_watermark: u64,
    pub doc_count: usize,
    pub generation: u64,
}

pub struct CompletedShardReindex {
    pub shard_id: ShardId,
    pub staging_root: PathBuf,
    pub built_through: u64,
    pub generation: u64,
}

pub struct ReindexJob {
    pub params: ReindexParams,
    pub shard_tx: Sender<ShardCmd>,
    pub progress: Arc<ReindexProgress>,
    pub stats: Arc<DbStats>,
}

/// One worker by default: a rebuild saturates disk and CPU, so running several
/// at once makes the whole database slower rather than faster.
pub struct ReindexPool {
    tx: Sender<ReindexJob>,
    joins: Vec<JoinHandle<()>>,
}

impl ReindexPool {
    pub fn start(workers: usize) -> Self {
        // let workers = workers.max(workers);
        let (tx, rx) = bounded::<ReindexJob>(64);
        let mut joins = Vec::with_capacity(workers);

        for i in 0..workers {
            let rx = rx.clone();
            joins.push(
                thread::Builder::new()
                    .name(format!("reindex-{i}"))
                    .spawn(move || worker_loop(rx))
                    .expect("failed to spawn reindex worker"),
            );
        }
        Self { tx, joins }
    }

    #[timed(reindex)]
    pub fn submit(&self, job: ReindexJob) -> Result<(), CorelamoError> {
        self.tx
            .send(job)
            .map_err(|_| CorelamoError::Internal("reindex pool is not running".into()))
    }

    pub fn shutdown(self) {
        drop(self.tx);
        for j in self.joins {
            let _ = j.join();
        }
    }
}

fn worker_loop(rx: Receiver<ReindexJob>) {
    while let Ok(job) = rx.recv() {
        let started = Instant::now();

        let done = match build_staging_index(&job.params, &job.progress, &job.stats) {
            Ok(done) => done,
            Err(_) => {
                job.stats.finish_shard_reindex(false, started.elapsed());
                continue;
            }
        };

        // hand the finished build back to the thread that owns the shard state
        let (rtx, rrx) = bounded(1);
        if job
            .shard_tx
            .send(ShardCmd::CommitReindex { done, resp: rtx })
            .is_err()
        {
            job.stats.finish_shard_reindex(false, started.elapsed());
            continue;
        }
        let ok = matches!(rrx.recv(), Ok(Ok(())));
        job.stats.finish_shard_reindex(ok, started.elapsed());
    }
}

/// Builds a fresh index into index.new. Reads the shard's document store but
/// touches nothing the shard thread owns.
#[timed(reindex)]
fn build_staging_index(
    params: &ReindexParams,
    progress: &ReindexProgress,
    _stats: &DbStats,
) -> Result<CompletedShardReindex, CorelamoError> {
    let staging_root = params.shard_root.join("index.new");
    if staging_root.exists() {
        std::fs::remove_dir_all(&staging_root)?;
    }
    std::fs::create_dir_all(&staging_root)?;

    let store = BinaryDocumentStore::open(params.shard_root.join("documents"))?;
    let index = LsmIndex::persistent(&staging_root, params.options.runtime.flush_threshold)?;
    let mut staging = SearchDatabase::with_shard_policy(
        store,
        index,
        Analyzer::new(),
        params.policy.clone(),
        params.shard_id,
    )
    .map_err(|e| CorelamoError::Internal(e.to_string()))?;

    staging
        .reindex_existing_documents(
            params.options.runtime.indexing_batch_size,
            params.options.runtime.indexing_window_size,
            progress,
        )
        .map_err(|e| CorelamoError::Internal(e.to_string()))?;

    staging
        .shutdown()
        .map_err(|e| CorelamoError::Internal(e.to_string()))?;

    Ok(CompletedShardReindex {
        shard_id: params.shard_id,
        staging_root,
        built_through: params.wal_watermark,
        generation: params.generation,
    })
}

