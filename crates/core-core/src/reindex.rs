use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use ahash::{HashMap, HashMapExt};
use core_index::disk::reader::DiskSegment;
use core_index::vector::{VectorFile, VectorFileWriter, vector_file_path};
use crossbeam_channel::{Receiver, Sender, bounded};

use core_index::analyzer::Analyzer;
use core_index::document::IndexPolicy;
use core_index::lsm::LsmIndex;
use core_index::lsm::index_worker::ReindexProgress;
use core_index::types::{DocId, ShardId, XPathId};
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
                thread::Builder::new()
                    .name(format!("reindex-{i}"))
                    .spawn(move || worker_loop(rx))
                    .expect("failed to spawn reindex worker"),
            );
        }
        Self { tx, joins }
    }

    #[timed(reindex)]
    pub fn submit(&self, job: PendingReindexJob) -> Result<(), CorelamoError> {
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

fn worker_loop(rx: Receiver<PendingReindexJob>) {
    while let Ok(job) = rx.recv() {
        let started = Instant::now();

        let params = match job.rx.recv() {
            Ok(Ok(p)) => p,
            Ok(Err(_)) | Err(_) => {
                job.stats.finish_shard_reindex(false, started.elapsed());
                continue;
            }
        };

        job.stats.add_reindex_total(params.doc_count as u64);

        let generation = params.generation;

        let done = match build_staging_index(&params, &job.progress, &job.stats) {
            Ok(done) => done,
            Err(_) => {
                // The build failed or was cancelled. `prepare_reindex` took this shard's
                // compaction worker for the duration of the attempt, so hand it back.
                abandon(&job, generation);
                job.stats.finish_shard_reindex(false, started.elapsed());
                continue;
            }
        };

        let (rtx, rrx) = bounded(1);
        if job
            .shard_tx
            .send(ShardCmd::CommitReindex { done, resp: rtx })
            .is_err()
        {
            // the shard thread is gone, and its worker went with it
            job.stats.finish_shard_reindex(false, started.elapsed());
            continue;
        }
        let ok = matches!(rrx.recv(), Ok(Ok(())));
        if !ok {
            // rejected (a newer attempt took over) or failed halfway; the shard ignores this
            // when the generation has moved on
            abandon(&job, generation);
        }
        job.stats.finish_shard_reindex(ok, started.elapsed());
    }
}

fn abandon(job: &PendingReindexJob, generation: u64) {
    let _ = job.shard_tx.send(ShardCmd::ReindexAbandoned { generation });
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
    // vectors are derived data: copying them only saves re-embedding, so a failure is not fatal
    let vector_xpaths = params.policy.vector_xpaths();
    if let Err(e) = carry_vectors(
        &params.shard_root.join("index"),
        &staging_root,
        &vector_xpaths,
    ) {
        eprintln!(
            "[reindex] shard {}: vectors not carried, they will be embedded again: {e}",
            params.shard_id
        );
    }
    Ok(CompletedShardReindex {
        shard_id: params.shard_id,
        staging_root,
        built_through: params.wal_watermark,
        generation: params.generation,
    })
}
/// Copies the vectors of documents that survive the rebuild from the live index into vector
/// files for the staging segments. Doc ids and xpath ids are the same in both indexes, and the
/// text of a document does not change in a reindex, so the rows stay valid. Only opens live
/// files: the live index may still finish one compaction or embedding round meanwhile.
fn carry_vectors(live_root: &Path, staging_root: &Path, xpaths: &[XPathId]) -> io::Result<usize> {
    if xpaths.is_empty() {
        return Ok(0);
    }
    let live_segments = segment_files(live_root)?;
    let staging_segments = segment_files(staging_root)?;
    let mut carried = 0;
    for &xpath in xpaths {
        let opened: Vec<VectorFile> = live_segments
            .iter()
            .filter_map(|segment| VectorFile::open(vector_file_path(segment, xpath)).ok())
            .collect();
        let Some(model) = dominant_model(&opened) else {
            continue;
        };
        // rows of another model are stale; the background worker re-embeds those documents
        let sources: Vec<VectorFile> = opened
            .into_iter()
            .filter(|file| file.model_id() == model && !file.is_empty())
            .collect();
        let (dims, scale) = (sources[0].dims(), sources[0].scale());
        let mut rows: HashMap<DocId, (usize, usize)> = HashMap::new();
        for (source, file) in sources.iter().enumerate() {
            for index in 0..file.len() {
                rows.insert(file.doc_id(index), (source, index));
            }
        }
        for segment in &staging_segments {
            carried += write_carried(segment, xpath, &model, dims, scale, &sources, &rows)?;
        }
    }
    Ok(carried)
}

/// The model id most rows were made with, if any rows exist.
fn dominant_model(files: &[VectorFile]) -> Option<String> {
    let mut rows_per_model: HashMap<&str, usize> = HashMap::new();
    for file in files {
        *rows_per_model.entry(file.model_id()).or_default() += file.len();
    }
    rows_per_model
        .into_iter()
        .filter(|(_, rows)| *rows > 0)
        .max_by_key(|(_, rows)| *rows)
        .map(|(model, _)| model.to_string())
}

fn write_carried(
    segment: &Path,
    xpath: XPathId,
    model: &str,
    dims: usize,
    scale: f32,
    sources: &[VectorFile],
    rows: &HashMap<DocId, (usize, usize)>,
) -> io::Result<usize> {
    let disk = DiskSegment::open(segment)?;
    let mut docs: Vec<DocId> = disk
        .doc_lengths()
        .iter()
        .filter(|((_, field), _)| *field == xpath)
        .map(|((doc_id, _), _)| doc_id)
        .filter(|doc_id| rows.contains_key(doc_id))
        .collect();
    docs.sort_unstable();
    docs.dedup();
    if docs.is_empty() {
        return Ok(0);
    }
    let mut writer =
        VectorFileWriter::create(vector_file_path(segment, xpath), model, dims, scale)?;
    for doc_id in &docs {
        let (source, index) = rows[doc_id];
        writer.push(*doc_id, &sources[source].row(index))?;
    }
    writer.finish()?;
    Ok(docs.len())
}

/// `segment-N.idx` files of an index directory, by name.
fn segment_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        let is_segment = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("segment-") && name.ends_with(".idx"));
        if is_segment {
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}
