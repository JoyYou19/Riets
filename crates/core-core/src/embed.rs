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

use core_embed::{EmbedLease, QUANT_SCALE};
use core_index::{
    disk::reader::DiskSegment,
    document::IndexPolicy,
    lsm::{index_worker::IndexCommand, snapshot::IndexSnapshot},
    search::SearchIndex,
    types::{DocId, XPathId},
    vector::{VectorFile, VectorFileWriter, vector_file_path},
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
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            max_docs_per_round: 100_000,
            round_time_budget: Duration::from_secs(300),
            embed_batch: 1024,
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

pub struct EmbeddingJob {
    sender: Sender<IndexCommand>,
    shared: Arc<SharedShardState>,
    policy: IndexPolicy,
    fields: Vec<VectorField>,
    config: EmbeddingConfig,
    log: Logger,
    /// Documents whose text could not be loaded; not retried until the process restarts.
    skipped: HashSet<DocId>,
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
            skipped: HashSet::new(),
        })
    }

    /// One bounded round. Ok(None): nothing to embed. Ok(Some(n)): `n` documents embedded
    /// (0 when every candidate was skipped, so the caller may call again right away).
    pub fn step(&mut self, lease: &mut EmbedLease<'_>, stop: &AtomicBool) -> io::Result<Option<usize>> {
        let vector_id = lease.vector_id().to_string();
        let dims = lease.spec().dims;
        let Some(task) = self.next_task(&vector_id, dims)? else {
            return Ok(None);
        };
        self.embed_task(lease, task, stop).map(Some)
    }

    /// (documents that need a vector, documents that have one) across the live segments,
    /// ignoring deleted documents. Skipped documents count as done: they cannot be embedded.
    pub fn measure(&self, vector_id: &str, dims: usize) -> io::Result<(u64, u64)> {
        let snapshot = self.shared.snapshot.get();
        let (mut total, mut done) = (0u64, 0u64);
        for segment in disk_segments(&self.sender)? {
            let disk = DiskSegment::open(&segment)?;
            for field in &self.fields {
                let existing = existing_file(&snapshot, &segment, field.xpath, vector_id, dims);
                for ((doc_id, xpath), _) in disk.doc_lengths().iter() {
                    if xpath != field.xpath || snapshot.is_deleted(doc_id) {
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

    /// Largest segment first, so the bulk of the corpus gets vectors before small new segments.
    fn next_task(&self, vector_id: &str, dims: usize) -> io::Result<Option<Task>> {
        let snapshot = self.shared.snapshot.get();
        let mut segments = disk_segments(&self.sender)?;
        segments.sort_by_key(|path| {
            std::cmp::Reverse(std::fs::metadata(path).map(|m| m.len()).unwrap_or(0))
        });

        for segment in segments {
            let disk = DiskSegment::open(&segment)?;
            for field in &self.fields {
                let existing = existing_file(&snapshot, &segment, field.xpath, vector_id, dims);
                let mut missing: Vec<DocId> = disk
                    .doc_lengths()
                    .iter()
                    .filter(|((_, xpath), _)| *xpath == field.xpath)
                    .map(|((doc_id, _), _)| doc_id)
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

    fn embed_task(&mut self, lease: &mut EmbedLease<'_>, task: Task, stop: &AtomicBool) -> io::Result<usize> {
        let started = Instant::now();
        let mut new_rows: Vec<(DocId, Vec<i8>)> = Vec::with_capacity(task.missing.len());

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
            let vectors = lease.embed_documents(&refs)?;
            new_rows.extend(doc_ids.into_iter().zip(vectors));
        }

        if new_rows.is_empty() {
            return Ok(0);
        }
        let added = new_rows.len();
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
        Ok(added)
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
    mut new_rows: Vec<(DocId, Vec<i8>)>,
) -> io::Result<()> {
    new_rows.sort_unstable_by_key(|(doc_id, _)| *doc_id);
    let mut writer =
        VectorFileWriter::create(vector_file_path(segment, xpath), vector_id, dims, QUANT_SCALE)?;
    let mut fresh = new_rows.into_iter().peekable();

    if let Some(existing) = existing {
        for index in 0..existing.len() {
            let doc_id = existing.doc_id(index);
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
            writer.push(doc_id, &existing.row(index))?;
        }
    }
    for (doc_id, row) in fresh {
        writer.push(doc_id, &row)?;
    }
    writer.finish()
}

fn disk_segments(sender: &Sender<IndexCommand>) -> io::Result<Vec<PathBuf>> {
    let (reply, rx) = mpsc::channel();
    sender
        .send(IndexCommand::DiskSegments { reply })
        .map_err(|_| broken_pipe("index worker stopped"))?;
    rx.recv()
        .map_err(|_| broken_pipe("index worker dropped disk segment reply"))?
}

fn install_vectors(sender: &Sender<IndexCommand>, segment: PathBuf, xpath: XPathId) -> io::Result<bool> {
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

fn broken_pipe(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, message)
}

