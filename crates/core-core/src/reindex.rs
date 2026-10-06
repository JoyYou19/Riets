use std::io;
use std::path::{ Path, PathBuf };
use std::sync::Arc;
use std::thread::{ self, JoinHandle };
use std::time::Instant;

use core_index::array_rows::ArrayRowAllocator;
use core_storage::json_indexing_helper::index_document;
use core_storage::json_parse::parse_source_into_node;
use core_storage::search_database::for_each_snapshot_document;
use crossbeam_channel::{ Receiver, Sender, bounded };

use core_index::analyzer::Analyzer;
use core_index::document::{ IndexPolicy, IndexedDocument };
use core_index::lsm::LsmIndex;
use core_index::lsm::index_worker::ReindexProgress;
use core_index::types::ShardId;
use core_protocol::errors::CorelamoError;
use core_storage::binary_store::DocLocation;
use core_timing::timed;

use crate::metrics::DbStats;
use crate::{ DatabaseOptions, shard_worker::ShardCmd };

pub struct ReindexParams {
    pub shard_id: ShardId,
    pub shard_root: PathBuf,
    pub policy: IndexPolicy,
    pub options: DatabaseOptions,
    pub wal_watermark: u64,
    pub doc_count: usize,
    pub generation: u64,
    pub locations: Vec<DocLocation>,
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
pub struct PendingReindexJob {
    pub rx: Receiver<Result<ReindexParams, CorelamoError>>,
    pub shard_tx: Sender<ShardCmd>,
    pub progress: Arc<ReindexProgress>,
    pub stats: Arc<DbStats>,
}
/// One worker by default: a rebuild saturates disk and CPU, so running several
/// at once makes the whole database slower rather than faster.
pub struct ReindexPool {
    tx: Sender<PendingReindexJob>,
    joins: Vec<JoinHandle<()>>,
}

impl ReindexPool {
    pub fn start(workers: usize) -> Self {
        // let workers = workers.max(workers);
        let (tx, rx) = bounded::<PendingReindexJob>(64);
        let mut joins = Vec::with_capacity(workers);

        for i in 0..workers {
            let rx = rx.clone();
            joins.push(
                thread::Builder
                    ::new()
                    .name(format!("reindex-{i}"))
                    .spawn(move || worker_loop(rx))
                    .expect("failed to spawn reindex worker")
            );
        }
        Self { tx, joins }
    }

    #[timed(reindex)]
    pub fn submit(&self, job: PendingReindexJob) -> Result<(), CorelamoError> {
        self.tx.send(job).map_err(|_| CorelamoError::Internal("reindex pool is not running".into()))
    }

    pub fn shutdown(self) {
        drop(self.tx);
        for j in self.joins {
            let _ = j.join();
        }
    }
}

fn worker_loop(rx: Receiver<PendingReindexJob>) {
    while let Ok(job) = rx.recv() {
        let started = Instant::now();

        let mut params = match job.rx.recv() {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                // eprintln!("[reindex] prepare failed: {e:?}");
                job.stats.finish_shard_reindex(false, started.elapsed());
                continue;
            }
            Err(_) => {
                // eprintln!("[reindex] shard dropped the prepare reply");
                job.stats.finish_shard_reindex(false, started.elapsed());
                continue;
            }
        };

        job.stats.add_reindex_total(params.doc_count as u64);

        let done = match build_staging_index(&mut params, &job.progress, &job.stats) {
            Ok(done) => done,
            Err(e) => {
                // eprintln!("[reindex] shard {:?} build failed: {e:?}", params.shard_id);
                job.stats.finish_shard_reindex(false, started.elapsed());
                continue;
            }
        };

        let (rtx, rrx) = bounded(1);
        if job.shard_tx.send(ShardCmd::CommitReindex { done, resp: rtx }).is_err() {
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
#[timed(reindex)]
fn build_staging_index(
    params: &mut ReindexParams,
    progress: &ReindexProgress,
    _stats: &DbStats,
) -> Result<CompletedShardReindex, CorelamoError> {
    if params.doc_count > 0 && params.locations.is_empty() {
        return Err(CorelamoError::Internal(format!(
            "shard {:?}: store has {} documents but the location snapshot is empty",
            params.shard_id, params.doc_count
        )));
    }

    let staging_root = params.shard_root.join("index.new");
    if staging_root.exists() {
        std::fs::remove_dir_all(&staging_root)?;
    }
    std::fs::create_dir_all(&staging_root)?;

    let started = std::time::Instant::now();
    // eprintln!(
    //     "[reindex] shard {:?}: {} locations in snapshot, {} documents in store",
    //     params.shard_id,
    //     params.locations.len(),
    //     params.doc_count,
    // );

    let built = (|| -> Result<(), CorelamoError> {
        let mut index = LsmIndex::persistent(&staging_root, params.options.runtime.flush_threshold)?;
        build_index_from_snapshot(
            &mut index,
            &Analyzer::new(),
            &params.policy,
            &params.shard_root.join("documents"),
            std::mem::take(&mut params.locations),
            params.options.runtime.indexing_batch_size,
            progress,
        )?;

        if params.doc_count > 0 && index.segment_count() == 0 {
            return Err(CorelamoError::Internal(format!(
                "shard {:?}: rebuild produced no segments for {} documents",
                params.shard_id, params.doc_count
            )));
        }
        Ok(())
    })();

    if let Err(e) = built {
        let _ = std::fs::remove_dir_all(&staging_root);
        return Err(e);
    }

    // eprintln!("[reindex] shard {:?}: built in {:.2?}", params.shard_id, started.elapsed());

    Ok(CompletedShardReindex {
        shard_id: params.shard_id,
        staging_root,
        built_through: params.wal_watermark,
        generation: params.generation,
    })
}
/// Rebuilds an index from a document-location snapshot.
/// A reader thread loads and parses batches while the calling thread indexes them;
/// at most 2 parsed batches wait in between. No store, no snapshot publishing.
pub fn build_index_from_snapshot(
    index: &mut LsmIndex,
    analyzer: &Analyzer,
    policy: &IndexPolicy,
    documents_dir: &Path,
    locations: Vec<DocLocation>,
    batch_size: usize,
    progress: &ReindexProgress
) -> io::Result<()> {
    let batch_size = batch_size.max(1);
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<IndexedDocument>>(2);

    std::thread::scope(
        |scope| -> io::Result<()> {
            let reader = scope.spawn(
                move || -> io::Result<()> {
                    let mut allocator = ArrayRowAllocator::default(); // ADAPT: your allocator type/constructor
                    let mut batch: Vec<IndexedDocument> = Vec::with_capacity(batch_size);

                    for_each_snapshot_document(
                        documents_dir,
                        locations,
                        &mut (|doc| {
                            if progress.is_cancelled() {
                                return Err(io::Error::other("reindex cancelled"));
                            }
                            let node = match parse_source_into_node(&doc.source, policy) {
                                Ok(node) => node,
                                Err(e) => {
                                    // eprintln!(
                                    //     "[reindex] skipping document {}: {e:?}",
                                    //     doc.external_id
                                    // );
                                    return Ok(());
                                }
                            };
                            batch.push(
                                index_document(doc.internal_id, &node, policy, &mut allocator)
                            );

                            if batch.len() >= batch_size {
                                let full = std::mem::replace(
                                    &mut batch,
                                    Vec::with_capacity(batch_size)
                                );
                                tx
                                    .send(full)
                                    .map_err(|_| io::Error::other("reindex indexer stopped"))?;
                            }
                            Ok(())
                        })
                    )?;

                    if !batch.is_empty() {
                        tx.send(batch).map_err(|_| io::Error::other("reindex indexer stopped"))?;
                    }
                    Ok(())
                }
            );

            let mut index_result = Ok(());
            for batch in rx.iter() {
                let indexed = batch.len() as u64;
                let (_, result) = index.add_indexed_documents(analyzer, &batch);
                if let Err(e) = result {
                    index_result = Err(e);
                    break;
                }
                progress.add(indexed);
            }
            // Dropping the receiver unblocks the reader if indexing stopped early.
            drop(rx);

            let read_result = reader
                .join()
                .map_err(|_| io::Error::other("reindex reader thread panicked"))?;
            index_result?;
            read_result
        }
    )?;

    index.flush()
}
