//! Embedding of `$vector` fields for one shard, one bounded round at a time.
//!
//! The caller must guarantee that no index compaction runs during `step()`: the shard's
//! background worker runs both on one thread. A round finds the largest segment with documents
//! that have a vector field but no vector, loads their text from the document store, embeds
//! it, rewrites that segment's vector file with the new rows added, and installs it.

use std::{
    collections::{BTreeMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    time::{Duration, Instant},
};

use ahash::{HashMap, HashMapExt};
use core_embed::EmbedLease;
use core_index::{
    disk::reader::DiskSegment,
    document::IndexPolicy,
    lsm::{index_worker::IndexCommand, snapshot::IndexSnapshot},
    search::SearchIndex,
    types::{DocId, XPathId},
    vector::{MIN_ROWS_FOR_GRAPH, VectorFile, VectorFileWriter, vector_file_path},
};
use core_storage::json_parse::parse_source_into_node;
use serde::{Deserialize, Serialize};
use slog::{Logger, warn};

use crate::shared_state::SharedShardState;

/// Per-database embedding work settings, stored in the database's config.toml.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddingConfig {
    /// Upper bound on documents considered in one round.
    pub max_docs_per_round: usize,
    /// A round starts no new batch after this long; this shard's compaction waits at most
    /// about this long.
    pub round_time_budget: Duration,
    /// Documents loaded and handed to the model per call; stop and time limits are checked
    /// between calls.
    pub embed_batch: usize,
    /// While documents are being loaded in bulk, wait for the load to end (and for the shard
    /// to be merged into one segment) before embedding. Off: embed while loading.
    pub wait_for_load: bool,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            max_docs_per_round: 10000,
            round_time_budget: Duration::from_secs(120),
            embed_batch: 1024,
            wait_for_load: true,
        }
    }
}

impl EmbeddingConfig {
    /// Replaces values that would stop embedding from making progress.
    fn sanitized(self) -> Self {
        let defaults = Self::default();
        let max_docs_per_round = if self.max_docs_per_round == 0 {
            defaults.max_docs_per_round
        } else {
            self.max_docs_per_round
        };
        Self {
            max_docs_per_round,
            round_time_budget: if self.round_time_budget.is_zero() {
                defaults.round_time_budget
            } else {
                self.round_time_budget
            },
            embed_batch: self.embed_batch.clamp(1, max_docs_per_round),
            wait_for_load: self.wait_for_load,
        }
    }
}

#[derive(Debug, Clone)]
struct VectorField {
    xpath: XPathId,
    path: String,
}

struct Task {
    segment: PathBuf,
    field: VectorField,
    /// Current vector file of this segment and field, if it was made by the loaded model.
    existing: Option<Arc<VectorFile>>,
    /// Documents with text in the field but no vector, ascending.
    missing: Vec<DocId>,
    /// Identifies the model and settings the new vectors are made with.
    vector_id: String,
    dims: usize,
}
type SegmentDocs = Vec<(XPathId, Vec<DocId>)>;
pub struct EmbeddingJob {
    sender: Sender<IndexCommand>,
    shared: Arc<SharedShardState>,
    policy: IndexPolicy,
    fields: Vec<VectorField>,
    config: EmbeddingConfig,
    log: Logger,
    segment_docs:HashMap<PathBuf,Arc<SegmentDocs>>,
    skipped: HashSet<DocId>,

    graph_failed: HashSet<u64>,
}

/// A vector file that is ready for an HNSW graph.
pub struct GraphTask {
    pub segment: PathBuf,
    pub xpath: XPathId,
    pub file: Arc<VectorFile>,
}

impl EmbeddingJob {
    /// Returns None when the policy has no vector fields.
    pub fn new(
        sender: Sender<IndexCommand>,
        shared: Arc<SharedShardState>,
        policy: IndexPolicy,
        config: EmbeddingConfig,
        log: Logger,
    ) -> Option<Self> {
        let fields: Vec<VectorField> = policy
            .vector_fields()
            .into_iter()
            .map(|field| VectorField {
                xpath: field.xpath(&policy),
                path: field.full_path.clone(),
            })
            .collect();
        if fields.is_empty() {
            return None;
        }
        Some(Self {
            sender,
            shared,
            policy,
            fields,
            config: config.sanitized(),
            log,
            segment_docs:HashMap::new(),
            skipped: HashSet::new(),
            graph_failed: HashSet::new(),

        })
    }

    /// Whether embedding waits while documents are loaded in bulk (`wait_for_load`).
    pub fn waits_for_load(&self) -> bool {
        self.config.wait_for_load
    }

    /// One bounded round. Ok(None): nothing to embed. Ok(Some(n)): `n` documents embedded
    /// (0 when every candidate was skipped, so the caller may call again right away).
    pub fn step(
        &mut self,
        lease: &mut EmbedLease<'_>,
        stop: &AtomicBool,
    ) -> io::Result<Option<usize>> {
        let vector_id = lease.vector_id().to_string();
        let dims = lease.spec().dims;
        let Some(task) = self.next_task(&vector_id, dims)? else {
            return Ok(None);
        };
        self.embed_task(lease, task, stop).map(Some)
    }

    /// (documents that need a vector, documents that have one) across the live segments,
    /// ignoring deleted documents. Skipped documents count as done: they cannot be embedded.
    pub fn measure(&mut self, vector_id: &str, dims: usize) -> io::Result<(u64, u64)> {
        let snapshot = self.shared.snapshot.get();
        let segments = disk_segments(&self.sender)?;
        self.forget_removed_segments(&segments);

        let (mut total, mut done) = (0u64, 0u64);
        for segment in &segments {
            let docs = self.segment_docs(segment)?;
            for (xpath, ids) in docs.iter() {
                let existing = existing_file(&snapshot, segment, *xpath, vector_id, dims);
                for &doc_id in ids {
                    if snapshot.is_deleted(doc_id) {
                        continue;
                    }
                    total += 1;
                    if self.skipped.contains(&doc_id)
                        || existing.as_ref().is_some_and(|file| file.contains(doc_id))
                    {
                        done += 1;
                    }
                }
            }
        }
        Ok((total, done))
    }

    /// The largest vector file that needs a graph: made with the current model, big enough
    /// (`MIN_ROWS_FOR_GRAPH`)
    pub fn next_graph(&mut self, vector_id: &str, dims: usize) -> io::Result<Option<GraphTask>> {
        let snapshot = self.shared.snapshot.get();
        let mut candidates: Vec<_> = snapshot
            .vectors()
            .iter()
            .filter(|entry| {
                entry.graph.is_none()
                    && entry.file.len() >= MIN_ROWS_FOR_GRAPH
                    && entry.file.matches(vector_id, dims)
                    && !self.graph_failed.contains(&entry.file.file_id())
                    && self.fields.iter().any(|field| field.xpath == entry.xpath)
            })
            .collect();
        candidates.sort_by_key(|entry| std::cmp::Reverse(entry.file.len()));

        for entry in candidates {
            let docs = match self.segment_docs(&entry.segment) {
                Ok(docs) => docs,
                // merged away since the snapshot was taken
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let complete = docs
                .iter()
                .filter(|(xpath, _)| *xpath == entry.xpath)
                .flat_map(|(_, ids)| ids.iter().copied())
                .all(|doc_id| {
                    snapshot.is_deleted(doc_id)
                        || self.skipped.contains(&doc_id)
                        || entry.file.contains(doc_id)
                });
            if complete {
                return Ok(Some(GraphTask {
                    segment: entry.segment.clone(),
                    xpath: entry.xpath,
                    file: Arc::clone(&entry.file),
                }));
            }
        }
        Ok(None)
    }

    /// Stops `next_graph` from offering this vector file again (its build failed).
    pub fn graph_failed(&mut self, file_id: u64) {
        self.graph_failed.insert(file_id);
    }

  
     fn next_task(&mut self, vector_id: &str, dims: usize) -> io::Result<Option<Task>> {
        let snapshot = self.shared.snapshot.get();
        let mut segments = disk_segments(&self.sender)?;
        self.forget_removed_segments(&segments);
        segments.sort_by_key(|path| {
            std::cmp::Reverse(std::fs::metadata(path).map(|m| m.len()).unwrap_or(0))
        });

        for segment in segments {
            let docs = self.segment_docs(&segment)?;
            for (xpath, ids) in docs.iter() {
                let Some(field) = self.fields.iter().find(|f| f.xpath == *xpath) else {
                    continue;
                };
                let existing = existing_file(&snapshot, &segment, *xpath, vector_id, dims);
                let mut missing: Vec<DocId> = ids
                    .iter()
                    .copied()
                    .filter(|doc_id| {
                        !snapshot.is_deleted(*doc_id)
                            && !self.skipped.contains(doc_id)
                            && existing.as_ref().is_none_or(|file| !file.contains(*doc_id))
                    })
                    .take(self.config.max_docs_per_round)
                    .collect();
                if missing.is_empty() {
                    continue;
                }
                missing.sort_unstable();
                missing.dedup();
                return Ok(Some(Task {
                    segment,
                    field: field.clone(),
                    existing,
                    missing,
                    vector_id: vector_id.to_string(),
                    dims,
                }));
            }
        }
        Ok(None)
    }

    fn embed_task(
        &mut self,
        lease: &mut EmbedLease<'_>,
        task: Task,
        stop: &AtomicBool,
    ) -> io::Result<usize> {
        let started = Instant::now();
        let mut new_rows: Vec<(DocId, Vec<f32>)> = Vec::with_capacity(task.missing.len());
        // the error that ended the round early; the vectors made before it are still installed
        let mut stopped_by: Option<io::Error> = None;

        for batch in task.missing.chunks(self.config.embed_batch) {
            if stop.load(Ordering::Relaxed) || started.elapsed() >= self.config.round_time_budget {
                break;
            }
            let mut doc_ids = Vec::with_capacity(batch.len());
            let mut texts = Vec::with_capacity(batch.len());
            for &doc_id in batch {
                match self.load_text(doc_id, &task.field.path)? {
                    Some(text) => {
                        doc_ids.push(doc_id);
                        texts.push(text);
                    }
                    None => {
                        self.skipped.insert(doc_id);
                    }
                }
            }
            if texts.is_empty() {
                continue;
            }
            let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
            let mut failed: Vec<(DocId, io::Error)> = Vec::new();
            let embedded = embed_isolating(
                &doc_ids,
                &refs,
                &mut |part: &[&str]| lease.embed_documents(part),
                &mut failed,
            );
            for (doc_id, error) in failed {
                warn!(self.log, "document cannot be embedded; skipped until restart";
                    "doc_id" => doc_id, "field" => &task.field.path, "error" => %error);
                self.skipped.insert(doc_id);
            }
            match embedded {
                Ok(rows) => new_rows.extend(rows),
                Err(error) => {
                    stopped_by = Some(error);
                    break;
                }
            }
        }

        let added = new_rows.len();
        if added > 0 {
            write_vectors(
                &task.segment,
                task.field.xpath,
                task.existing.as_deref(),
                &task.vector_id,
                task.dims,
                new_rows,
            )?;
            if !install_vectors(&self.sender, task.segment.clone(), task.field.xpath)? {
                warn!(self.log, "segment disappeared before its vectors were installed";
                    "segment" => %task.segment.display());
                return Ok(0);
            }
        }
        match stopped_by {
            Some(error) if added == 0 => Err(error),
            Some(error) => {
                warn!(self.log, "embedding round ended early; its vectors so far are kept";
                    "embedded" => added, "error" => %error);
                Ok(added)
            }
            None => Ok(added),
        }
    }

    /// The field's text, or None when the document is gone, unparsable, or the field is empty.
    fn load_text(&self, doc_id: DocId, path: &str) -> io::Result<Option<String>> {
        let Some(doc) = self.shared.read_document_by_doc_id(doc_id)? else {
            return Ok(None);
        };
        let Ok(node) = parse_source_into_node(&doc.source, &self.policy) else {
            return Ok(None);
        };
        let mut leaves = BTreeMap::new();
        node.collect_leaf_values(&mut leaves);
        Ok(leaves.remove(path).filter(|text| !text.trim().is_empty()))
    }
    fn segment_docs(&mut self, segment: &Path) -> io::Result<Arc<SegmentDocs>> {
        if let Some(docs) = self.segment_docs.get(segment) {
            return Ok(Arc::clone(docs));
        }
        let disk = DiskSegment::open(segment)?;
        let docs: SegmentDocs = self
            .fields
            .iter()
            .map(|field| {
                let ids = disk
                    .doc_lengths()
                    .iter()
                    .filter(|((_, xpath), _)| *xpath == field.xpath)
                    .map(|((doc_id, _), _)| doc_id)
                    .collect();
                (field.xpath, ids)
            })
            .collect();
        let docs = Arc::new(docs);
        self.segment_docs
            .insert(segment.to_path_buf(), Arc::clone(&docs));
        Ok(docs)
    }

    /// Drops cached segments that compaction removed.
    fn forget_removed_segments(&mut self, live: &[PathBuf]) {
        self.segment_docs.retain(|path, _| live.contains(path));
    }
}

/// A text every working model embeds: a part that fails alone while this works is the part's
/// own problem, not the model's.
const PROBE_TEXT: &str = "ok";

/// Embeds `texts` (one row per id). When the model fails on a group of texts, the group is
/// split in two until the texts it fails on are alone. Such a text is reported in `failed` and
/// left out, provided the model still embeds `PROBE_TEXT`; if it does not, the model itself is
/// failing and its error is returned instead.
fn embed_isolating<E>(
    ids: &[DocId],
    texts: &[&str],
    embed: &mut E,
    failed: &mut Vec<(DocId, io::Error)>,
) -> io::Result<Vec<(DocId, Vec<f32>)>>
where
    E: FnMut(&[&str]) -> io::Result<Vec<Vec<f32>>>,
{
    match embed(texts) {
        Ok(vectors) if vectors.len() == texts.len() => {
            Ok(ids.iter().copied().zip(vectors).collect())
        }
        Ok(vectors) => Err(io::Error::other(format!(
            "model returned {} vectors for {} texts",
            vectors.len(),
            texts.len()
        ))),
        Err(error) if texts.len() == 1 => {
            if embed(&[PROBE_TEXT]).is_err() {
                return Err(error);
            }
            failed.push((ids[0], error));
            Ok(Vec::new())
        }
        Err(_) => {
            let middle = texts.len() / 2;
            let mut rows = embed_isolating(&ids[..middle], &texts[..middle], embed, failed)?;
            rows.extend(embed_isolating(
                &ids[middle..],
                &texts[middle..],
                embed,
                failed,
            )?);
            Ok(rows)
        }
    }
}

/// The segment's current vector file for `xpath`, or None when it has none or it was made by
/// a different model or settings (then the segment is embedded again from scratch).
fn existing_file(
    snapshot: &IndexSnapshot,
    segment: &Path,
    xpath: XPathId,
    vector_id: &str,
    dims: usize,
) -> Option<Arc<VectorFile>> {
    snapshot
        .vectors()
        .iter()
        .find(|entry| entry.segment == segment && entry.xpath == xpath)
        .map(|entry| Arc::clone(&entry.file))
        .filter(|file| file.matches(vector_id, dims))
}

/// Writes `<segment>.<xpath>.vec` holding the existing rows plus `new_rows`, replacing the old
/// file atomically. A row in `new_rows` replaces an existing row for the same document.
fn write_vectors(
    segment: &Path,
    xpath: XPathId,
    existing: Option<&VectorFile>,
    vector_id: &str,
    dims: usize,
    mut new_rows: Vec<(DocId, Vec<f32>)>,
) -> io::Result<()> {
    new_rows.sort_unstable_by_key(|(doc_id, _)| *doc_id);
    let mut writer = VectorFileWriter::create(vector_file_path(segment, xpath), vector_id, dims)?;
    let mut fresh = new_rows.into_iter().peekable();

    if let Some(existing) = existing {
        for (index, &doc_id) in existing.doc_ids().iter().enumerate() {
            while let Some((next, _)) = fresh.peek() {
                if *next >= doc_id {
                    break;
                }
                let (next, row) = fresh.next().expect("peeked");
                writer.push(next, &row)?;
            }
            if fresh.peek().is_some_and(|(next, _)| *next == doc_id) {
                continue;
            }
            writer.push(doc_id, existing.original(index))?;
        }
    }
    for (doc_id, row) in fresh {
        writer.push(doc_id, &row)?;
    }
    writer.finish()?;
    Ok(())
}

fn disk_segments(sender: &Sender<IndexCommand>) -> io::Result<Vec<PathBuf>> {
    let (reply, rx) = mpsc::channel();
    sender
        .send(IndexCommand::DiskSegments { reply })
        .map_err(|_| broken_pipe("index worker stopped"))?;
    rx.recv()
        .map_err(|_| broken_pipe("index worker dropped disk segment reply"))?
}

fn install_vectors(
    sender: &Sender<IndexCommand>,
    segment: PathBuf,
    xpath: XPathId,
) -> io::Result<bool> {
    let (reply, rx) = mpsc::channel();
    sender
        .send(IndexCommand::InstallVectors {
            segment,
            xpath,
            reply,
        })
        .map_err(|_| broken_pipe("index worker stopped"))?;
    rx.recv()
        .map_err(|_| broken_pipe("index worker dropped install vectors reply"))?
}

/// Attaches a built graph; false when the segment or its vector file changed meanwhile.
pub fn install_graph(
    sender: &Sender<IndexCommand>,
    segment: PathBuf,
    xpath: XPathId,
) -> io::Result<bool> {
    let (reply, rx) = mpsc::channel();
    sender
        .send(IndexCommand::InstallGraph {
            segment,
            xpath,
            reply,
        })
        .map_err(|_| broken_pipe("index worker stopped"))?;
    rx.recv()
        .map_err(|_| broken_pipe("index worker dropped install graph reply"))?
}

fn broken_pipe(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, message)
}

