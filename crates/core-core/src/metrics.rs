use crate::shard_db::DatabaseStats;
use core_backup::progress::{ BackupPhase, BackupProgress };
use core_index::lsm::index_worker::{ IndexingStats, Phase, ReindexProgress };
use std::sync::{ Arc, OnceLock };
use std::sync::atomic::{ AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering::Relaxed };
use std::time::{ Duration, Instant };

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatabaseMetrics {
    pub search_requests: u64,
    pub search_errors: u64,
    pub search_total_time: Duration,
    pub indexing_requests: u64,
    pub indexing_errors: u64,
    pub indexing_total_time: Duration,
    pub reindex_requests: u64,
    pub reindex_errors: u64,
    pub reindex_total_time: Duration,
}

/// What a shard's background worker is doing right now.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackgroundActivity {
    #[default]
    Idle = 0,
    Compacting = 1,
    Embedding = 2,
}

impl BackgroundActivity {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Compacting,
            2 => Self::Embedding,
            _ => Self::Idle,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingStatus {
    /// No `$vector` fields in the policy.
    Disabled,
    /// Some documents still need vectors.
    Embedding,
    /// Every document with a vector field has one (or could not be embedded).
    Complete,
}

impl EmbeddingStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Embedding => "embedding",
            Self::Complete => "complete",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingStats {
    pub status: EmbeddingStatus,
    pub progress: u8,
    pub vectors_total: u64,
    pub vectors_done: u64,
    pub docs_per_second: Option<u64>,
    pub eta_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BackgroundStats {
    pub shards_compacting: usize,
    pub shards_embedding: usize,
    pub compaction_failures: u64,
    /// Duration of the longest most recent compaction across shards.
    pub last_compaction_ms: Option<u64>,
    pub embedding: EmbeddingStats,
}

#[derive(Debug)]
pub struct DbStats {
    counters: Counters,
    shards: Vec<ShardGauges>,
    /// One progress for the whole database: it starts when the first shard
    /// begins building and settles when the last one commits.
    reindex: Arc<ReindexProgress>,
    reindex_outstanding: AtomicUsize,
    reindex_failed: AtomicBool,
    backup: Arc<BackupProgress>,
    backup_outstanding: AtomicUsize,
    backup_failed: AtomicBool,
    restore: Arc<BackupProgress>,
    /// When the first embedding round that embedded anything started; the database-wide rate
    /// is measured from here because all shards share one model.
    embedding_started: OnceLock<Instant>,

}

/// Counters written by shard threads and the manager. Relaxed throughout:
/// these guard no other memory, only themselves.
#[derive(Debug, Default)]
struct Counters {
    search_requests: AtomicU64,
    search_errors: AtomicU64,
    search_nanos: AtomicU64,
    indexing_requests: AtomicU64,
    indexing_errors: AtomicU64,
    indexing_nanos: AtomicU64,
    reindex_requests: AtomicU64,
    reindex_errors: AtomicU64,
    reindex_nanos: AtomicU64,
}

#[derive(Debug, Default)]
struct ShardGauges {
    documents: AtomicUsize,
    segments: AtomicUsize,
    memtable_terms: AtomicUsize,
    compaction_enabled: AtomicBool,
    // cumulative counters that the shard thread updates
    documents_indexed: AtomicU64,
    documents_deleted: AtomicU64,
    segments_written: AtomicU64,
    compactions_completed: AtomicU64,
    // background worker
    activity: AtomicU8,
    compaction_failures: AtomicU64,
    last_compaction_ms: AtomicU64,
    embedding_enabled: AtomicBool,
    vectors_total: AtomicU64,
    vectors_measured:AtomicBool,
    vectors_done: AtomicU64,
    embedded: AtomicU64,
}

#[derive(Debug, Clone)]
pub struct ShardStatsHandle {
    stats: Arc<DbStats>,
    index: usize,
}

impl DatabaseMetrics {
    pub fn average_search_time(&self) -> Option<Duration> {
        if self.search_requests == 0 {
            return None;
        }
        Some(self.search_total_time / (self.search_requests as u32))
    }

    pub fn average_indexing_time(&self) -> Option<Duration> {
        if self.indexing_requests == 0 {
            return None;
        }
        Some(self.indexing_total_time / (self.indexing_requests as u32))
    }
}

impl DbStats {
    pub fn new(shard_count: usize) -> Arc<Self> {
        Arc::new(Self {
            counters: Counters::default(),
            shards: (0..shard_count).map(|_| ShardGauges::default()).collect(),
            reindex: ReindexProgress::new(),
            reindex_outstanding: AtomicUsize::new(0),
            reindex_failed: AtomicBool::new(false),
            backup: BackupProgress::new(),
            backup_outstanding: AtomicUsize::new(0),
            backup_failed: AtomicBool::new(false),
            restore: BackupProgress::new(),
            embedding_started: OnceLock::new(),
        })
    }

    pub fn handle(self: &Arc<Self>, shard_index: usize) -> ShardStatsHandle {
        ShardStatsHandle {
            stats: Arc::clone(self),
            index: shard_index,
        }
    }

    pub fn try_begin_restore(&self) -> bool {
        self.restore.try_begin()
    }

    pub fn finish_restore(&self, ok: bool) {
        self.restore.set_phase(if ok { BackupPhase::Complete } else { BackupPhase::Failed });
    }

    pub fn finish_backup(&self, ok: bool) {
        self.backup.set_phase(if ok { BackupPhase::Complete } else { BackupPhase::Failed });
    }

    pub fn backup_progress(&self) -> &Arc<BackupProgress> {
        &self.backup
    }

    pub fn reindex_progress(&self) -> &Arc<ReindexProgress> {
        &self.reindex
    }

    // ---- writes that are not shard-local ----

    /// Search fans out to every shard, so counting it per shard would multiply
    /// one request by the shard count. The manager records it once.
    pub fn record_search(&self, failed: bool, elapsed: Duration) {
        let c = &self.counters;
        c.search_requests.fetch_add(1, Relaxed);
        c.search_nanos.fetch_add(elapsed.as_nanos() as u64, Relaxed);
        if failed {
            c.search_errors.fetch_add(1, Relaxed);
        }
    }

    /// An insert fans out across shards too, so the manager records it once
    /// rather than each shard counting the same request.
    pub fn record_indexing(&self, failed: bool, elapsed: Duration) {
        let c = &self.counters;
        c.indexing_requests.fetch_add(1, Relaxed);
        c.indexing_nanos.fetch_add(elapsed.as_nanos() as u64, Relaxed);
        if failed {
            c.indexing_errors.fetch_add(1, Relaxed);
        }
    }

    fn record_reindex_request(&self) {
        self.counters.reindex_requests.fetch_add(1, Relaxed);
    }

    // ---- reindex lifecycle ----

    pub fn begin_reindex(&self, shard_count: usize) -> bool {
        if !self.reindex.try_begin(0) {
            return false;
        }
        self.reindex_failed.store(false, Relaxed);
        self.reindex_outstanding.store(shard_count, Relaxed);
        self.record_reindex_request();
        true
    }

    /// Called per shard as its ticket arrives, so the percentage is meaningful
    /// while the first shard is already building.
    pub fn add_reindex_total(&self, documents: u64) {
        self.reindex.grow_total(documents);
    }

    /// The last shard to finish settles the phase for the database.
    pub fn finish_shard_reindex(&self, ok: bool, elapsed: Duration) {
        let c = &self.counters;
        c.reindex_nanos.fetch_add(elapsed.as_nanos() as u64, Relaxed);
        if !ok {
            c.reindex_errors.fetch_add(1, Relaxed);
            self.reindex_failed.store(true, Relaxed);
        }
        if self.reindex_outstanding.fetch_sub(1, Relaxed) == 1 {
            if self.reindex.is_cancelled() {
                self.reindex.reset();
            } else {
                self.reindex.set_phase(
                    if self.reindex_failed.load(Relaxed) {
                        Phase::Failed
                    } else {
                        Phase::Complete
                    }
                );
            }
        }
    }

    // ---- backup lifecycle ----

    pub fn begin_backup(&self, shard_count: usize) -> bool {
        if !self.backup.try_begin() {
            return false;
        }
        self.backup_failed.store(false, Relaxed);
        self.backup_outstanding.store(shard_count, Relaxed);
        true
    }

    pub fn finish_shard_backup(&self, ok: bool) {
        if !ok {
            self.backup_failed.store(true, Relaxed);
        }
        if self.backup_outstanding.fetch_sub(1, Relaxed) == 1 {
            self.backup.set_phase(
                if self.backup_failed.load(Relaxed) {
                    BackupPhase::Failed
                } else {
                    BackupPhase::Complete
                }
            );
        }
    }

    // ---- reads ----

    pub fn metrics(&self) -> DatabaseMetrics {
        let c = &self.counters;
        DatabaseMetrics {
            search_requests: c.search_requests.load(Relaxed),
            search_errors: c.search_errors.load(Relaxed),
            search_total_time: Duration::from_nanos(c.search_nanos.load(Relaxed)),
            indexing_requests: c.indexing_requests.load(Relaxed),
            indexing_errors: c.indexing_errors.load(Relaxed),
            indexing_total_time: Duration::from_nanos(c.indexing_nanos.load(Relaxed)),
            reindex_requests: c.reindex_requests.load(Relaxed),
            reindex_errors: c.reindex_errors.load(Relaxed),
            reindex_total_time: Duration::from_nanos(c.reindex_nanos.load(Relaxed)),
        }
    }

    pub fn snapshot(&self) -> DatabaseStats {
        let mut documents = 0usize;
        let mut segments = 0usize;
        let mut terms = 0usize;
        let mut compaction = false;
        let mut indexed = 0u64;
        let mut deleted = 0u64;
        let mut written = 0u64;
        let mut compactions = 0u64;

        let mut compacting = 0usize;
        let mut embedding_now = 0usize;
        let mut compaction_failures = 0u64;
        let mut last_compaction_ms = 0u64;
        let mut embedding_enabled = false;
        let mut vectors_total = 0u64;
        let mut vectors_done = 0u64;
        let embedded = 0u64;
        let mut all_measured=true;
        for g in &self.shards {
            let doc_gauge = g.documents.load(Relaxed) as u64;
            let doc_indexed = g.documents_indexed.load(Relaxed);
            if g.embedding_enabled.load(Relaxed) && !g.vectors_measured.load(Relaxed) {
                all_measured = false;
            }
            // documents gauge is only refreshed by publish() (start/clear/reindex);
            // documents_indexed is a live running counter updated on every insert.
            // Use whichever is larger so status reflects live inserts without
            // needing a reindex/restart to refresh the gauge.
            documents += doc_gauge.max(doc_indexed) as usize; 
            segments += g.segments.load(Relaxed);
            terms += g.memtable_terms.load(Relaxed);
            compaction |= g.compaction_enabled.load(Relaxed);
            indexed += doc_indexed;
            deleted += g.documents_deleted.load(Relaxed);
            written += g.segments_written.load(Relaxed);
            compactions += g.compactions_completed.load(Relaxed);
            
            match BackgroundActivity::from_u8(g.activity.load(Relaxed)) {
                BackgroundActivity::Compacting => {
                    compacting += 1;
                }
                BackgroundActivity::Embedding => {
                    embedding_now += 1;
                }
                BackgroundActivity::Idle => {}
            }
            compaction_failures += g.compaction_failures.load(Relaxed);
            last_compaction_ms = last_compaction_ms.max(g.last_compaction_ms.load(Relaxed));
            embedding_enabled |= g.embedding_enabled.load(Relaxed);
            vectors_total += g.vectors_total.load(Relaxed);
            vectors_done += g.vectors_done.load(Relaxed);
           
        }

        DatabaseStats {
            document_count: documents,
            segment_count: segments,
            background_compaction_enabled: compaction,
            shard_count: self.shards.len(),

            metrics: self.metrics(),
            indexing: IndexingStats {
                total_documents_indexed: indexed,
                total_documents_deleted: deleted,
                segments_written: written,
                compactions_completed: compactions,
                memtable_term_count: terms,
                segment_count: segments,
            },
            backup: self.backup.snapshot().into(),
            restoring: false,
            reindexing: self.reindex.snapshot().into(),
            background: BackgroundStats {
                shards_compacting: compacting,
                shards_embedding: embedding_now,
                compaction_failures,
                last_compaction_ms: (last_compaction_ms > 0).then_some(last_compaction_ms),
                embedding: self.embedding_stats(
                    embedding_enabled,
                    vectors_total,
                    vectors_done,
                    embedded,
                    all_measured
                ),
            },
        }
    }

    fn embedding_stats(&self, enabled: bool, total: u64, done: u64, embedded: u64, measured:bool) -> EmbeddingStats {
        let done = done.min(total);
        let status = if !enabled {
            EmbeddingStatus::Disabled
        } else if measured && done >= total {
            EmbeddingStatus::Complete
        } else {
            EmbeddingStatus::Embedding
        };
        let progress = match status {
            EmbeddingStatus::Disabled => 0,
            _ if total == 0 => 100,
            _ => (done.saturating_mul(100) / total).min(100) as u8,
        };
        let rate = self.embedding_started
            .get()
            .map(|started| started.elapsed().as_secs_f64())
            .filter(|secs| *secs >= 2.0)
            .map(|secs| (embedded as f64) / secs)
            .filter(|rate| rate.is_finite() && *rate > 0.0);
        let eta_seconds = match (status, rate) {
            (EmbeddingStatus::Embedding, Some(rate)) =>
                Some((((total - done) as f64) / rate).ceil() as u64),
            _ => None,
        };
        EmbeddingStats {
            status,
            progress,
            vectors_total: total,
            vectors_done: done,
            docs_per_second: rate.map(|rate| rate.round() as u64),
            eta_seconds,
        }
    }
    
}

impl ShardStatsHandle {
    pub fn reindex_progress(&self) -> &Arc<ReindexProgress> {
        &self.stats.reindex
    }

    pub fn backup_progress(&self) -> &Arc<BackupProgress> {
        &self.stats.backup
    }

    pub fn add_documents_indexed(&self, n: u64) {
        self.stats.shards[self.index].documents_indexed.fetch_add(n, Relaxed);
    }

    /// Levels, so publish after anything that changes the index. The shard
    /// thread pays for reading these so the HTTP reader never has to.
    pub fn publish(&self, document_count: usize, stats: &IndexingStats) {
        let g = &self.stats.shards[self.index];
        g.documents.store(document_count, Relaxed);
        g.segments.store(stats.segment_count, Relaxed);
        g.memtable_terms.store(stats.memtable_term_count, Relaxed);
        g.documents_indexed.store(stats.total_documents_indexed, Relaxed);
        g.documents_deleted.store(stats.total_documents_deleted, Relaxed);
        g.segments_written.store(stats.segments_written, Relaxed);
        g.compactions_completed.store(stats.compactions_completed, Relaxed);
    }

    pub fn set_compaction_enabled(&self, on: bool) {
        self.stats.shards[self.index].compaction_enabled.store(on, Relaxed);
    }

    // ---- background worker ----

    pub fn set_activity(&self, activity: BackgroundActivity) {
        self.stats.shards[self.index].activity.store(activity as u8, Relaxed);
    }

    pub fn record_compaction(&self, ok: bool, elapsed: Duration) {
        let g = &self.stats.shards[self.index];
        if ok {
            g.last_compaction_ms.store(elapsed.as_millis() as u64, Relaxed);
        } else {
            g.compaction_failures.fetch_add(1, Relaxed);
        }
    }

     pub fn set_embedding_enabled(&self, on: bool) {
        let g = &self.stats.shards[self.index];
        g.embedding_enabled.store(on, Relaxed);
        // a (re)started worker must measure again before its figures count
        g.vectors_measured.store(false, Relaxed);
    }

    pub fn set_vector_progress(&self, total: u64, done: u64) {
        let g = &self.stats.shards[self.index];
        g.vectors_total.store(total, Relaxed);
        g.vectors_done.store(done, Relaxed);
        g.vectors_measured.store(true, Relaxed);
    }

    /// `round_started` anchors the database-wide rate at the first round that embedded anything.
    pub fn add_embedded(&self, count: u64, round_started: Instant) {
        self.stats.embedding_started.get_or_init(|| round_started);
        self.stats.shards[self.index].embedded.fetch_add(count, Relaxed);
    }
    
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_status_follows_progress() {
        let stats = DbStats::new(2);
        let (a, b) = (stats.handle(0), stats.handle(1));

        assert_eq!(stats.snapshot().background.embedding.status, EmbeddingStatus::Disabled);

        a.set_embedding_enabled(true);
        b.set_embedding_enabled(true);
        a.set_vector_progress(100, 100);
        b.set_vector_progress(300, 100);
        let embedding = stats.snapshot().background.embedding;
        assert_eq!(embedding.status, EmbeddingStatus::Embedding);
        assert_eq!((embedding.vectors_total, embedding.vectors_done, embedding.progress), (400, 200, 50));

        b.set_vector_progress(300, 300);
        let embedding = stats.snapshot().background.embedding;
        assert_eq!(embedding.status, EmbeddingStatus::Complete);
        assert_eq!(embedding.progress, 100);
        assert_eq!(embedding.eta_seconds, None);
    }

    #[test]
    fn activity_and_compaction_results_are_aggregated() {
        let stats = DbStats::new(3);
        stats.handle(0).set_activity(BackgroundActivity::Compacting);
        stats.handle(1).set_activity(BackgroundActivity::Embedding);
        stats.handle(2).record_compaction(false, Duration::from_millis(5));
        stats.handle(2).record_compaction(true, Duration::from_millis(1200));

        let background = stats.snapshot().background;
        assert_eq!((background.shards_compacting, background.shards_embedding), (1, 1));
        assert_eq!(background.compaction_failures, 1);
        assert_eq!(background.last_compaction_ms, Some(1200));
    }
}