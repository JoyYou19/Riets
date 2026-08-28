use core_timing::timed;
use rayon::prelude::*;
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    analyzer::Analyzer,
    document::IndexedDocument,
    lsm::{
        LsmIndex,
        compaction::{CompactionConfig, CompactionJob, CompletedCompaction},
        snapshot::SharedIndexSnapshot,
    },
    mem::MemIndex,
    segment::ImmutableSegment,
    types::DocId,
};

type Acknowledgement = Sender<io::Result<()>>;
type CompactionPlanReply = Sender<io::Result<Option<CompactionJob>>>;
type SegmentCountReply = Sender<io::Result<usize>>;

// Operate through commands
pub enum IndexCommand {
    AddIndexedDocument {
        document: IndexedDocument,
        ack: Option<Acknowledgement>,
    },
    DeleteDocument {
        doc_id: DocId,
        ack: Option<Acknowledgement>,
    },
    AddSegment {
        segment: ImmutableSegment,
        doc_count: u64,
        ack: Option<Acknowledgement>,
    },
    Flush {
        ack: Option<Acknowledgement>,
    },

    PlanCompaction {
        config: CompactionConfig,
        reply: CompactionPlanReply,
    },
    InstallCompaction {
        completed: CompletedCompaction,
        ack: Option<Acknowledgement>,
    },
    SegmentCount {
        reply: SegmentCountReply,
    },
    GetStats {
        reply: Sender<io::Result<IndexingStats>>,
    },
    Abort,
    Shutdown,
}

pub struct IndexWorker {
    sender: Sender<IndexCommand>,
    handle: Option<JoinHandle<io::Result<LsmIndex>>>,
}

impl IndexWorker {
    pub fn start(index: LsmIndex, analyzer: Analyzer, shared: SharedIndexSnapshot) -> Self {
        shared.publish(index.snapshot());

        let (sender, receiver) = mpsc::channel();

        let handle = thread::spawn(move || run_index_worker(index, analyzer, shared, receiver));

        Self {
            sender,
            handle: Some(handle),
        }
    }

    pub fn sender(&self) -> Sender<IndexCommand> {
        self.sender.clone()
    }

    pub fn send(&self, command: IndexCommand) -> io::Result<()> {
        self.sender
            .send(command)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "index worker stopped"))
    }

    pub fn shutdown(mut self) -> io::Result<LsmIndex> {
        self.send(IndexCommand::Shutdown)?;

        let handle = self
            .handle
            .take()
            .ok_or_else(|| io::Error::other("index worker already joined"))?;
        handle
            .join()
            .map_err(|_| io::Error::other("index worker panicked"))?
    }

    // Fire and forget functions
    pub fn add_indexed_document(&self, document: IndexedDocument) -> io::Result<()> {
        self.send(IndexCommand::AddIndexedDocument {
            document,
            ack: None,
        })
    }

    pub fn delete_document(&self, doc_id: DocId) -> io::Result<()> {
        self.send(IndexCommand::DeleteDocument { doc_id, ack: None })
    }

    pub fn add_segment(&self, segment: ImmutableSegment, doc_count: u64) -> io::Result<()> {
        self.send(IndexCommand::AddSegment {
            segment,
            doc_count,
            ack: None,
        })
    }

    pub fn flush(&self) -> io::Result<()> {
        self.send(IndexCommand::Flush { ack: None })
    }

    #[timed(compaction)]
    pub fn plan_compaction(&self, config: CompactionConfig) -> io::Result<Option<CompactionJob>> {
        let (reply, rx) = mpsc::channel();

        self.send(IndexCommand::PlanCompaction { config, reply })?;

        rx.recv().map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "index worker dropped compaction plan reply",
            )
        })?
    }

    // Waiting functions
    #[timed(indexing_documents)]
    pub fn add_indexed_document_wait(&self, document: IndexedDocument) -> io::Result<()> {
        let (ack, rx) = mpsc::channel();

        self.send(IndexCommand::AddIndexedDocument {
            document,
            ack: Some(ack),
        })?;

        wait_for_acknowledgement(rx)
    }

    #[timed(flushing)]
    pub fn flush_wait(&self) -> io::Result<()> {
        let (ack, rx) = mpsc::channel();

        self.send(IndexCommand::Flush { ack: Some(ack) })?;

        wait_for_acknowledgement(rx)
    }

    #[timed(modifying_documents)]
    pub fn delete_document_wait(&self, doc_id: DocId) -> io::Result<()> {
        let (ack, rx) = mpsc::channel();

        self.send(IndexCommand::DeleteDocument {
            doc_id,
            ack: Some(ack),
        })?;

        wait_for_acknowledgement(rx)
    }

    #[timed(indexing_documents)]
    pub fn add_segment_wait(&self, segment: ImmutableSegment, doc_count: u64) -> io::Result<()> {
        let (ack, rx) = mpsc::channel();

        self.send(IndexCommand::AddSegment {
            segment,
            doc_count,
            ack: Some(ack),
        })?;

        wait_for_acknowledgement(rx)
    }

    #[timed(compaction)]
    pub fn install_compaction_wait(&self, completed: CompletedCompaction) -> io::Result<()> {
        let (ack, rx) = mpsc::channel();

        self.send(IndexCommand::InstallCompaction {
            completed,
            ack: Some(ack),
        })?;

        wait_for_acknowledgement(rx)
    }

    pub fn abort(&self) -> io::Result<()> {
        self.send(IndexCommand::Abort)
    }

    pub fn install_compaction(&self, completed: CompletedCompaction) -> io::Result<()> {
        self.send(IndexCommand::InstallCompaction {
            completed,
            ack: None,
        })
    }

    pub fn segment_count(&self) -> io::Result<usize> {
        let (reply, rx) = mpsc::channel();
        self.send(IndexCommand::SegmentCount { reply })?;

        rx.recv().map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "index worker dropped segment count reply",
            )
        })?
    }
    //stats
    pub fn get_stats(&self) -> io::Result<IndexingStats> {
        let (reply, rx) = mpsc::channel();
        self.send(IndexCommand::GetStats { reply })?;
        rx.recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "stats receiver dropped"))?
    }
}

const PUBLISH_DOC_THRESHOLD: u64 = 100;
const PUBLISH_INTERVAL: Duration = Duration::from_millis(20);

fn run_index_worker(
    mut index: LsmIndex,
    analyzer: Analyzer,
    shared: SharedIndexSnapshot,
    receiver: Receiver<IndexCommand>,
) -> io::Result<LsmIndex> {
    let mut stats = IndexingStats::default();
    let mut docs_since_publish: u64 = 0;
    let mut last_publish = Instant::now();
    loop {
        let timeout = PUBLISH_INTERVAL.saturating_sub(last_publish.elapsed());

        let command = match receiver.recv_timeout(timeout) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if docs_since_publish > 0 {
                    shared.publish(index.snapshot());
                    docs_since_publish = 0;
                    last_publish = Instant::now();
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        match command {
            IndexCommand::AddIndexedDocument { document, ack } => {
                let outcome = index.add_indexed_document(&analyzer, &document);
                let ok = outcome.is_ok();
                if ok {
                    stats.total_documents_indexed += 1;
                }
                send_acknowledgement(ack, outcome)?;
                if ok {
                    docs_since_publish += 1;
                    maybe_publish_on_threshold(
                        &shared,
                        &index,
                        &mut docs_since_publish,
                        &mut last_publish,
                    );
                }
            }
            IndexCommand::DeleteDocument { doc_id, ack } => {
                let outcome = index.delete_document(doc_id);
                let ok = outcome.is_ok();
                if ok {
                    stats.total_documents_deleted += 1;
                }
                send_acknowledgement(ack, outcome)?;
                if ok {
                    shared.publish(index.snapshot());
                    docs_since_publish = 0;
                    last_publish = Instant::now();
                }
            }
            IndexCommand::AddSegment {
                segment,
                doc_count,
                ack,
            } => {
                let outcome = index.add_immutable_segment(segment);
                let ok = outcome.is_ok();
                if ok {
                    stats.segments_written += 1;
                    stats.total_documents_indexed += doc_count;
                }
                send_acknowledgement(ack, outcome)?;
                if ok {
                    docs_since_publish += doc_count;
                    maybe_publish_on_threshold(
                        &shared,
                        &index,
                        &mut docs_since_publish,
                        &mut last_publish,
                    );
                }
            }
            IndexCommand::PlanCompaction { config, reply } => {
                let result = index.plan_compaction(config);

                reply.send(result).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "compaction plan receiver dropped",
                    )
                })?;
            }

            IndexCommand::InstallCompaction { completed, ack } => {
                let outcome = index.install_compaction(completed);
                let installed = matches!(&outcome, Ok(true));
                let result = outcome.map(|_| ());
                send_acknowledgement(ack, result)?;
                if installed {
                    shared.publish(index.snapshot());
                    docs_since_publish = 0;
                    last_publish = Instant::now();
                    stats.compactions_completed += 1;
                }
            }
            IndexCommand::SegmentCount { reply } => {
                reply.send(Ok(index.segment_count())).map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "segment count receiver dropped")
                })?;
            }
            IndexCommand::Flush { ack } => {
                let outcome = index.flush();
                let ok = outcome.is_ok();
                send_acknowledgement(ack, outcome)?;
                if ok {
                    shared.publish(index.snapshot());
                    docs_since_publish = 0;
                    last_publish = Instant::now();
                }
            }
            IndexCommand::Shutdown => {
                index.flush()?;
                shared.publish(index.snapshot());
                return Ok(index);
            }
            IndexCommand::Abort => {
                return Ok(index);
            }
            IndexCommand::GetStats { reply } => {
                stats.segment_count = index.segment_count();
                stats.memtable_term_count = index.memtable_term_count();
                reply.send(Ok(stats.clone())).map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "stats receiver dropped")
                })?;
            }
        }
    }

    index.flush()?;
    Ok(index)
}

fn maybe_publish_on_threshold(
    shared: &SharedIndexSnapshot,
    index: &LsmIndex,
    docs_since_publish: &mut u64,
    last_publish: &mut Instant,
) {
    if *docs_since_publish >= PUBLISH_DOC_THRESHOLD {
        shared.publish(index.snapshot());
        *docs_since_publish = 0;
        *last_publish = Instant::now();
    }
}

fn wait_for_acknowledgement(rx: Receiver<io::Result<()>>) -> io::Result<()> {
    rx.recv().map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "index worker dropped acknowledgement",
        )
    })?
}

fn send_acknowledgement(
    acknowledgement: Option<Acknowledgement>,
    result: io::Result<()>,
) -> io::Result<()> {
    if let Some(acknowledgement) = acknowledgement {
        acknowledgement.send(result).map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "acknowledgement receiver dropped",
            )
        })?;
    }

    Ok(())
}

#[timed(indexing_documents)]
pub fn build_segment_batch(
    analyzer: &Analyzer,
    documents: Vec<IndexedDocument>,
) -> ImmutableSegment {
    let mut mem = MemIndex::new();

    for document in documents {
        mem.add_indexed_document(analyzer, &document);
    }

    mem.freeze()
}

#[timed(indexing_documents)]
pub fn index_batches_parallel(
    worker: &IndexWorker,
    analyzer: Analyzer,
    batches: Vec<Vec<IndexedDocument>>,
) -> io::Result<()> {
    let doc_counts: Vec<u64> = batches.iter().map(|batch| batch.len() as u64).collect();
    let segments: Vec<ImmutableSegment> = batches
        .into_par_iter()
        .map(|batch| build_segment_batch(&analyzer, batch))
        .collect();

    for (segment, doc_count) in segments.into_iter().zip(doc_counts) {
        worker.add_segment_wait(segment, doc_count)?;
    }

    Ok(())
}

#[timed(indexing_documents)]
pub fn make_batches<T>(items: Vec<T>, batch_size: usize) -> Vec<Vec<T>> {
    assert!(batch_size > 0, "batch_size must be greater than 0");
    let mut batches = Vec::new();
    let mut current = Vec::with_capacity(batch_size);

    for item in items {
        current.push(item);

        if current.len() == batch_size {
            batches.push(current);
            current = Vec::with_capacity(batch_size);
        }
    }

    if !current.is_empty() {
        batches.push(current);
    }

    batches
}

#[timed(indexing_documents)]
pub fn build_segments_parallel(
    analyzer: Analyzer,
    batches: Vec<Vec<IndexedDocument>>,
) -> Vec<ImmutableSegment> {
    batches
        .into_par_iter()
        .map(|batch| build_segment_batch(&analyzer, batch))
        .collect()
}
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexingStats {
    pub total_documents_indexed: u64,
    pub total_documents_deleted: u64,
    pub segments_written: u64,
    pub compactions_completed: u64,
    pub memtable_term_count: usize,
    pub segment_count: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReindexStatus {
    #[default]
    Idle,
    Reindexing,
    Swapping,
    CatchUp,
    Complete,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct ReindexingStats {
    pub status: ReindexStatus,
    pub progress: u8,
    pub documents_indexed: u64,
    pub eta_seconds: Option<u64>,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Idle = 0,
    Reindexing = 1,
    Swapping = 2,
    CatchUp = 3,
    Complete = 4,
    Failed = 5,
}

impl Phase {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Phase::Reindexing,
            2 => Phase::Swapping,
            3 => Phase::CatchUp,
            4 => Phase::Complete,
            5 => Phase::Failed,
            _ => Phase::Idle,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Reindexing => "reindexing",
            Phase::Swapping => "swapping",
            Phase::CatchUp => "catch_up",
            Phase::Complete => "complete",
            Phase::Failed => "failed",
        }
    }

    pub fn is_running(self) -> bool {
        matches!(self, Phase::Reindexing | Phase::Swapping | Phase::CatchUp)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ProgressSnapshot {
    pub phase: Phase,
    pub done: u64,
    pub total: u64,
    pub percent: u8,
    pub eta_seconds: Option<u64>,
}

impl From<ProgressSnapshot> for ReindexingStats {
    fn from(s: ProgressSnapshot) -> Self {
        ReindexingStats {
            status: match s.phase {
                Phase::Idle => ReindexStatus::Idle,
                Phase::Reindexing => ReindexStatus::Reindexing,
                Phase::Swapping => ReindexStatus::Swapping,
                Phase::CatchUp => ReindexStatus::CatchUp,
                Phase::Complete => ReindexStatus::Complete,
                Phase::Failed => ReindexStatus::Failed,
            },
            progress: s.percent,
            documents_indexed: s.done,
            eta_seconds: s.eta_seconds,
        }
    }
}

#[derive(Debug)]
pub struct ReindexProgress {
    phase: AtomicU8,
    total: AtomicU64,
    done: AtomicU64,
    cancel: AtomicBool,
    first_add: Mutex<Option<Instant>>,
}

impl ReindexProgress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            phase: AtomicU8::new(Phase::Idle as u8),
            total: AtomicU64::new(0),
            done: AtomicU64::new(0),
            first_add: Mutex::new(None),
            cancel: AtomicBool::new(false),
        })
    }

    /// Claims the reindex slot. Returns false if one is already running.
    pub fn try_begin(&self, total: u64) -> bool {
        loop {
            let current = self.phase.load(Ordering::Acquire);
            if Phase::from_u8(current).is_running() {
                return false;
            }
            match self.phase.compare_exchange_weak(
                current,
                Phase::Reindexing as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(_) => continue,
            }
        }
        self.total.store(total, Ordering::Relaxed);
        self.done.store(0, Ordering::Relaxed);
        if let Ok(mut t) = self.first_add.lock() {
            *t = None;
        }
        true
    }

    pub fn add(&self, docs: u64) {
        if self.done.fetch_add(docs, Ordering::Relaxed) == 0
            && let Ok(mut t) = self.first_add.lock()
        {
            t.get_or_insert_with(Instant::now);
        }
    }

    pub fn grow_total(&self, extra: u64) {
        self.total.fetch_add(extra, Ordering::Relaxed);
    }

    pub fn set_total(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
    }

    pub fn set_phase(&self, phase: Phase) {
        self.phase.store(phase as u8, Ordering::Release);
    }
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    pub fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::Acquire))
    }

    pub fn snapshot(&self) -> ProgressSnapshot {
        let phase = self.phase();
        let total = self.total.load(Ordering::Relaxed);
        let raw_done = self.done.load(Ordering::Relaxed);
        let done = if total > 0 {
            raw_done.min(total)
        } else {
            raw_done
        };

        let percent = if phase == Phase::Complete {
            100
        } else if total == 0 {
            0
        } else {
            ((done * 100) / total) as u8
        };

        ProgressSnapshot {
            phase,
            done,
            total,
            percent,
            eta_seconds: self.eta(phase, done, total),
        }
    }

    fn eta(&self, phase: Phase, done: u64, total: u64) -> Option<u64> {
        if !matches!(phase, Phase::Reindexing | Phase::CatchUp) {
            return None;
        }
        if done == 0 || total == 0 || done >= total {
            return None;
        }
        // One window's worth of work divided by a couple of seconds is noise.
        // Wait for 2% before publishing a number.
        if done.saturating_mul(50) < total {
            return None;
        }

        let started = (*self.first_add.lock().ok()?)?;
        let elapsed = started.elapsed().as_secs_f64();
        if elapsed < 2.0 {
            return None;
        }
        let rate = done as f64 / elapsed;
        if rate <= f64::EPSILON {
            return None;
        }
        let remaining = (total - done) as f64 / rate;
        if !remaining.is_finite() {
            return None;
        }
        Some(round_eta(remaining))
    }
}

fn round_eta(secs: f64) -> u64 {
    let s = secs.ceil().clamp(0.0, u32::MAX as f64) as u64;
    match s {
        0..=30 => s,
        31..=300 => s.div_ceil(5) * 5,
        _ => s.div_ceil(30) * 30,
    }
}

/// Panic-safe terminal state: if the reindex thread unwinds, the drop marks the
/// run failed instead of leaving the phase stuck at `Reindexing`.
pub struct ReindexGuard {
    progress: Arc<ReindexProgress>,
    settled: bool,
}

impl ReindexGuard {
    pub fn new(progress: Arc<ReindexProgress>) -> Self {
        Self {
            progress,
            settled: false,
        }
    }

    pub fn succeed(mut self) {
        self.progress.set_phase(Phase::Complete);
        self.settled = true;
    }

    pub fn fail(mut self) {
        self.progress.set_phase(Phase::Failed);
        self.settled = true;
    }
}

impl Drop for ReindexGuard {
    fn drop(&mut self) {
        if !self.settled {
            self.progress.set_phase(Phase::Failed);
        }
    }
}
