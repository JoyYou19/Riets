use std::sync::Arc;
use std::sync::atomic::{ AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed };
use std::time::Duration;
use crate::shard_db::DatabaseStats;
use core_backup::progress::{ BackupPhase, BackupProgress };
use core_index::lsm::index_worker::{ IndexingStats, Phase, ReindexProgress };
/// Read-only snapshot. Built fresh on every read, never stored.
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
    // documents_indexed: AtomicU64,
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
/// One slot per shard. Levels, not counters: the owning shard overwrites its
/// own slot and no other thread ever writes it.
#[derive(Debug, Default)]
struct ShardGauges {
    documents: AtomicUsize,
    segments: AtomicUsize,
    memtable_terms: AtomicUsize,
    compaction_enabled: AtomicBool,
    // Add cumulative counters that the shard thread updates
    documents_indexed: AtomicU64,
    documents_deleted: AtomicU64,
    segments_written: AtomicU64,
    compactions_completed: AtomicU64,
}
/// What one shard holds. Writes go to that shard's own gauge slot or to a
/// shared counter, so nothing here can block the shard thread.
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
            // documents_indexed: AtomicU64::new(0),
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
    pub fn restore_progress(&self) -> &BackupProgress {
        &self.restore
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
    // pub fn update_reindex_progress(&self, documents_indexed: u64) {
    //     self.reindex.add_indexed(documents_indexed);
    // }

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

    //backup related shit

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

    pub fn abort_backup(&self) {
        self.backup_outstanding.store(0, Relaxed);
        self.backup.set_phase(BackupPhase::Failed);
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

        for g in &self.shards {
            documents += g.documents.load(Relaxed);
            segments += g.segments.load(Relaxed);
            terms += g.memtable_terms.load(Relaxed);
            compaction |= g.compaction_enabled.load(Relaxed);
            indexed += g.documents_indexed.load(Relaxed);
            deleted += g.documents_deleted.load(Relaxed);
            written += g.segments_written.load(Relaxed);
            compactions += g.compactions_completed.load(Relaxed);
        }

        DatabaseStats {
            document_count: documents,
            segment_count: segments,
            background_compaction_enabled: compaction,
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
    // pub fn add_indexed(&self, n: u64) {
    //     self.stats.counters.indexing_requests.fetch_add(n, Relaxed);
    // }
    // pub fn add_deleted(&self, n: u64) {
    //     self.stats.counters.documents_deleted.fetch_add(n, Relaxed);
    // }

    pub fn set_compaction_enabled(&self, on: bool) {
        self.stats.shards[self.index].compaction_enabled.store(on, Relaxed);
    }
}
