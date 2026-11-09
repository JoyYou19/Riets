//! One background thread per shard. Work runs in priority order, one step at a time:
//! a submitted reindex (alone, after the current step finishes), index compaction, embedding,
//! HNSW graphs for fully embedded vector files, and, only when idle, document-store
//! compaction. Heavy CPU and disk work runs on
//! `background_pool()`, so compaction proceeds on as many shards at once as there are cores.

use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use core_embed::{Embedder, ModelRegistry};
use core_index::{
    background::background_pool,
    lsm::{
        compaction::{CompactionConfig, run_compaction_cycle},
        index_worker::{IndexCommand, ReindexProgress},
    },
    vector::{GraphBuildParams, build_graph_file, hnsw_file_path},
};
use core_protocol::errors::CorelamoError;
use core_storage::binary_store::run_segment_compaction;
use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, bounded, unbounded};
use slog::{Logger, error, info, warn};

use crate::{
    embed::{EmbeddingJob, install_graph},
    metrics::{BackgroundActivity, ShardStatsHandle},
    reindex::{ReindexTask, build_staging_index},
    shard_worker::ShardCmd,
};

const MEASURE_EVERY: Duration = Duration::from_secs(60);
/// How often a wait on the shard queue looks at the stop flag.
const STOP_POLL: Duration = Duration::from_millis(100);
/// A store-compaction check that finds nothing doubles its wait, up to interval × this.
const STORE_BACKOFF_MAX: u32 = 64;
pub(super) struct BackgroundConfig {
    pub index: Sender<IndexCommand>,
    pub shard_tx: crossbeam_channel::Sender<ShardCmd>,
    pub compaction: Option<CompactionConfig>,
    pub store_compaction: Option<f64>,
    pub interval: Duration,
    pub models: Arc<ModelRegistry>,
    pub embedding: Option<EmbeddingJob>,
    pub stats: ShardStatsHandle,
    pub log: Logger,
}
pub struct BackgroundWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<io::Result<()>>>,
    reindex_tx: crossbeam_channel::Sender<ReindexTask>,
    current: Arc<Mutex<Option<Arc<ReindexProgress>>>>,
}

impl BackgroundWorker {
    /// Returns None when there is nothing to run: compaction disabled and no vector fields.
    pub(crate) fn start(config: BackgroundConfig) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let current = Arc::new(Mutex::new(None));
        let (reindex_tx, reindex_rx) = unbounded();
        let looped = BackgroundLoop {
            store_wait: config.interval,
            next_store_check: Instant::now() + config.interval,
            config,
            reindex_rx,
            current: Arc::clone(&current),
            embedder: None,
            last_model_error: None,
            last_measure: None,
            measured: None,
        };
        let stop_thread = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("shard-background".to_string())
            .spawn(move || looped.run(&stop_thread))
            .expect("failed to spawn shard background worker");
        Self {
            stop,
            reindex_tx,
            current,
            handle: Some(handle),
        }
    }

    /// Queues a reindex. It starts once the current step finishes; nothing else starts before it.
    pub fn submit_reindex(&self, task: ReindexTask) -> Result<(), CorelamoError> {
        self.reindex_tx
            .send(task)
            .map_err(|_| CorelamoError::Internal("background worker is not running".into()))
    }
    fn signal_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(progress) = self.current.lock().ok().and_then(|c| c.clone()) {
            progress.cancel();
        }
    }
    /// Stops after the current step and waits for the thread.
    pub fn stop(mut self) -> io::Result<()> {
        self.signal_stop();
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .map_err(|_| io::Error::other("shard background worker panicked"))??;
        }
        Ok(())
    }

   
    pub fn stop_async(mut self) {
        self.signal_stop();
        self.handle.take();
    }
}

impl Drop for BackgroundWorker {
    fn drop(&mut self) {
        self.signal_stop();
    }
}
struct BackgroundLoop {
    config: BackgroundConfig,
    reindex_rx: Receiver<ReindexTask>,
    current: Arc<Mutex<Option<Arc<ReindexProgress>>>>,
    embedder: Option<Arc<Embedder>>,
    last_model_error: Option<String>,
    last_measure: Option<Instant>,
    measured: Option<(u64, u64)>,
    next_store_check: Instant,
    store_wait: Duration,
}
impl BackgroundLoop {
    fn run(mut self, stop: &AtomicBool) -> io::Result<()> {
        let result = self.run_loop(stop);
       
        while let Ok(task) = self.reindex_rx.try_recv() {
            let _ = std::fs::remove_dir_all(task.params.shard_root.join("index.new"));
            task.stats.finish_shard_reindex(false, Duration::ZERO);
        }
        result
    }

    fn run_loop(&mut self, stop: &AtomicBool) -> io::Result<()> {
        while !stop.load(Ordering::Relaxed) {
            // 1. a reindex: runs alone; the step that was running has already finished
            if let Ok(task) = self.reindex_rx.try_recv() {
                self.run_reindex(task, stop);
                continue;
            }
            // 2. index compaction, until nothing is due
            match self.compact_once(stop) {
                Ok(true) => {
                    continue;
                }
                Ok(false) => {}
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    return Ok(());
                }
                Err(_) => {
                    self.wait_for_work(stop, self.config.interval);
                    continue;
                }
            }
            // 3. embedding, one round, then re-check 1 and 2
            match self.embed_once(stop) {
                Ok(true) => {
                    continue;
                }
                Ok(false) => {}
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    return Ok(());
                }
                Err(e) => warn!(self.config.log, "embedding round failed"; "error" => %e),
            }
            // 4. one HNSW graph for a fully embedded vector file, then re-check 1 to 3
            match self.graph_once(stop) {
                Ok(true) => {
                    continue;
                }
                Ok(false) => {}
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    return Ok(());
                }
                Err(e) => warn!(self.config.log, "vector graph step failed"; "error" => %e),
            }
            // 5. idle: document-store compaction, rarely
            if self.store_compaction_due() {
                self.store_compaction_once(stop);
                continue;
            }
            self.wait_for_work(stop, self.config.interval);
        }
        Ok(())
    }

    fn wait_for_work(&mut self, stop: &AtomicBool, total: Duration) {
        let deadline = Instant::now() + total;
        while !stop.load(Ordering::Relaxed) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || !self.reindex_rx.is_empty() {
                return;
            }
            let _ = crossbeam_channel::after(STOP_POLL.min(left)).recv();
        }
    }

    fn compact_once(&mut self, _stop: &AtomicBool) -> io::Result<bool> {
        let Some(config) = self.config.compaction else {
            return Ok(false);
        };
        self.config
            .stats
            .set_activity(BackgroundActivity::Compacting);
        let started = Instant::now();
        let result = run_compaction_cycle(&self.config.index, config, &self.config.log);
        self.config.stats.set_activity(BackgroundActivity::Idle);
        match &result {
            Ok(true) => self.config.stats.record_compaction(true, started.elapsed()),
            Ok(false) => {}
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {}
            Err(e) => {
                self.config
                    .stats
                    .record_compaction(false, started.elapsed());
                error!(self.config.log, "index compaction cycle failed; retrying after interval"; "error" => %e);
            }
        }
        result
    }

     fn ask_shard<T>(&self, stop: &AtomicBool, cmd: ShardCmd, rx: &Receiver<T>) -> Option<T> {
        if !self.send_to_shard(stop, cmd) {
            return None;
        }
        loop {
            match rx.recv_timeout(STOP_POLL) {
                Ok(value) => return Some(value),
                Err(RecvTimeoutError::Timeout) if !stop.load(Ordering::Relaxed) => {}
                Err(_) => return None,
            }
        }
    }
  
    fn send_to_shard(&self, stop: &AtomicBool, cmd: ShardCmd) -> bool {
        let mut pending = cmd;
        loop {
            match self.config.shard_tx.send_timeout(pending, STOP_POLL) {
                Ok(()) => {
                    return true;
                }
                Err(SendTimeoutError::Timeout(cmd)) if !stop.load(Ordering::Relaxed) => {
                    pending = cmd;
                }
                Err(_) => {
                    return false;
                }
            }
        }
    }

    fn run_reindex(&mut self, task: ReindexTask, stop: &AtomicBool) {
        let ReindexTask {
            params,
            progress,
            stats,
        } = task;
        let started = Instant::now();
        let generation = params.generation;
        let shard_root = params.shard_root.clone();
        stats.add_reindex_total(params.doc_count as u64);
        if let Ok(mut current) = self.current.lock() {
            *current = Some(Arc::clone(&progress));
        }
        self.config
            .stats
            .set_activity(BackgroundActivity::Reindexing);

       
        let built = build_staging_index(&params, &progress, &stats);

        self.config.stats.set_activity(BackgroundActivity::Idle);
        if let Ok(mut current) = self.current.lock() {
            *current = None;
        }

        match built {
            Ok(done) => {
              
                let cmd = ShardCmd::CommitReindex {
                    done,
                    stats: Arc::clone(&stats),
                    started,
                };
                if self.send_to_shard(stop, cmd) {
                    while !stop.load(Ordering::Relaxed) {
                        thread::sleep(STOP_POLL);
                    }
                } else {
                    let _ = std::fs::remove_dir_all(shard_root.join("index.new"));
                    stats.finish_shard_reindex(false, started.elapsed());
                }
            }
            Err(e) => {
                warn!(self.config.log, "reindex build failed or was cancelled"; "error" => %e);
                let _ = std::fs::remove_dir_all(shard_root.join("index.new"));
                self.send_to_shard(stop, ShardCmd::ReindexAbandoned { generation });
                stats.finish_shard_reindex(false, started.elapsed());
            }
        }
    }

    fn store_compaction_due(&self) -> bool {
        self.config.store_compaction.is_some() && Instant::now() >= self.next_store_check
    }

    fn store_compaction_once(&mut self, stop: &AtomicBool) {
        let Some(dead_ratio_threshold) = self.config.store_compaction else {
            return;
        };
        let (reply, rx) = bounded(1);
        let planned = self.ask_shard(
            stop,
            ShardCmd::PlanSegmentCompaction {
                dead_ratio_threshold,
                reply,
            },
            &rx,
        );
        let Some(Ok(Some(job))) = planned else {
            // nothing to reclaim: wait twice as long next time
            let max = self.config.interval * STORE_BACKOFF_MAX;
            self.store_wait = (self.store_wait * 2).min(max);
            self.next_store_check = Instant::now() + self.store_wait;
            return;
        };
        self.config.stats.set_activity(BackgroundActivity::Cleanup);
        let completed = background_pool().install(|| run_segment_compaction(job));
        self.config.stats.set_activity(BackgroundActivity::Idle);
        match completed {
            Ok(completed) => {
                let (ack, rx) = bounded(1);
                let _ = self.ask_shard(
                    stop,
                    ShardCmd::InstallSegmentCompaction {
                        completed,
                        ack: Some(ack),
                    },
                    &rx,
                );
            }
            Err(e) => warn!(self.config.log, "store compaction failed"; "error" => %e),
        }
        // it found something: more may be due soon
        self.store_wait = self.config.interval;
        self.next_store_check = Instant::now() + self.store_wait;
    }
    
    fn embed_once(&mut self, stop: &AtomicBool) -> io::Result<bool> {
        if self.config.embedding.is_none() {
            return Ok(false);
        }
       
        let vector_id = self.config.models.default_vector_id();
        let dims = self.config.models.default_model().dims;
        if self
            .last_measure
            .is_none_or(|at| at.elapsed() >= MEASURE_EVERY)
        {
            self.measure(&vector_id, dims);
        }
        let waiting = self.waiting_for_load()?;
        self.config.stats.set_embedding_waiting(waiting);
        if waiting {
            return Ok(false);
        }
        let Some(embedder) = self.embedder() else {
            return Ok(false);
        };
        
        let Some(mut lease) = embedder.try_lease()? else {
            return Ok(false);
        };

        let round_started = Instant::now();
        self.config
            .stats
            .set_activity(BackgroundActivity::Embedding);
        let result = self
            .config
            .embedding
            .as_mut()
            .expect("checked above")
            .step(&mut lease, stop);
        drop(lease);
        if stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        self.config.stats.set_activity(BackgroundActivity::Idle);
        let did_work = match result {
            Err(e) => {
                self.config.stats.set_embedding_error(Some(e.to_string()));
                return Err(e);
            }
            Ok(None) => false,
            Ok(Some(count)) => {
                self.config.stats.set_embedding_error(None);
                if count > 0 {
                    self.config.stats.add_embedded(count as u64, round_started);
                    info!(self.config.log, "embedded documents"; "count" => count);
                }
                true
            }
        };
        if did_work {
            self.measure(&vector_id, dims);
        }
        Ok(did_work)
    }

    /// Builds the graph of one complete vector file (see `EmbeddingJob::next_graph`).
    /// Runs only when embedding had nothing to do this pass, so a file is not graphed and then
    /// rewritten by the next round. Returns true when it built or tried a graph.
    fn graph_once(&mut self, stop: &AtomicBool) -> io::Result<bool> {
        if self.config.embedding.is_none() || stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        // a bulk load is about to be merged: its segments, and their graphs, would go away
        if self.waiting_for_load()? {
            return Ok(false);
        }
        let vector_id = self.config.models.default_vector_id();
        let dims = self.config.models.default_model().dims;
        let Some(task) = self
            .config
            .embedding
            .as_mut()
            .expect("checked above")
            .next_graph(&vector_id, dims)?
        else {
            return Ok(false);
        };

        let path = hnsw_file_path(&task.segment, task.xpath);
        let started = Instant::now();
       
        self.config
            .stats
            .set_activity(BackgroundActivity::Embedding);
        
        let built = background_pool()
            .install(|| build_graph_file(&task.file, &path, &GraphBuildParams::default()));
        self.config.stats.set_activity(BackgroundActivity::Idle);

        match built {
            Ok(true) => {
                if install_graph(&self.config.index, task.segment.clone(), task.xpath)? {
                    info!(self.config.log, "built vector graph";
                        "segment" => %task.segment.display(),
                        "xpath" => task.xpath,
                        "rows" => task.file.len(),
                        "elapsed_ms" => started.elapsed().as_millis());
                }
              
                Ok(true)
            }
            Ok(false) => {
                // below MIN_ROWS_FOR_GRAPH; next_graph filters these, so only a size race
                self.mark_graph_failed(task.file.file_id());
                Ok(false)
            }
            Err(e) => {
                warn!(self.config.log, "building vector graph failed; searching by scan";
                    "segment" => %task.segment.display(), "xpath" => task.xpath, "error" => %e);
                self.mark_graph_failed(task.file.file_id());
                Ok(true)
            }
        }
    }

    fn mark_graph_failed(&mut self, file_id: u64) {
        if let Some(job) = self.config.embedding.as_mut() {
            job.graph_failed(file_id);
        }
    }

  
    fn waiting_for_load(&self) -> io::Result<bool> {
        let (Some(compaction), Some(job)) =
            (self.config.compaction, self.config.embedding.as_ref())
        else {
            return Ok(false);
        };
        if !job.waits_for_load() {
            return Ok(false);
        }
        // Nothing counted yet (a new shard): whatever arrives is the load.
        let (total, done) = self.measured.unwrap_or((0, 0));
        let missing = total.saturating_sub(done);
        if total > 0 && missing.saturating_mul(compaction.size_ratio.max(1)) < total {
            return Ok(false);
        }
        let (reply, rx) = std::sync::mpsc::channel();
        self.config
            .index
            .send(IndexCommand::Settling {
                config: compaction,
                reply,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "index worker stopped"))?;
        rx.recv().map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "index worker dropped settling reply",
            )
        })
    }

    fn measure(&mut self, vector_id: &str, dims: usize) {
         let job = self
            .config
            .embedding
            .as_mut()
            .expect("measure is only called with a job");
        match job.measure(vector_id, dims) {
            Ok((total, done)) => {
                self.measured = Some((total, done));
                self.config.stats.set_vector_progress(total, done);
            }
            Err(e) => warn!(self.config.log, "measuring embedding progress failed"; "error" => %e),
        }
        self.last_measure = Some(Instant::now());
    }

    fn embedder(&mut self) -> Option<Arc<Embedder>> {
        if let Some(embedder) = &self.embedder {
            return Some(Arc::clone(embedder));
        }
       
       
        match self.config.models.try_get_default() {
            Ok(Some(embedder)) => {
                match embedder.cpu_fallback() {
                    Some(reason) => {
                        warn!(self.config.log, "embedding model ready on the CPU; CUDA did not start";
                    "model" => embedder.spec().id,
                    "instances" => embedder.instances(),
                    "reason" => reason);
                    }
                    None => info!(self.config.log, "embedding model ready";
                    "model" => embedder.spec().id,
                    "device" => %embedder.device(),
                    "instances" => embedder.instances()),
                }
                self.config.stats.set_embedding_error(None);
                self.last_model_error = None;
                self.embedder = Some(Arc::clone(&embedder));
                Some(embedder)
            }
            Ok(None)=>None,
            Err(e) => {
                let message = e.to_string();
                if self.last_model_error.as_deref() != Some(message.as_str()) {
                    warn!(self.config.log, "embedding model unavailable; retrying later";
                        "error" => %message);
                }
                self.config.stats.set_embedding_error(Some(message.clone()));
                self.last_model_error = Some(message);
                None
            }
        }
    }
}
