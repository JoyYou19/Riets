//! One background thread per shard for maintenance that must never overlap: index compaction
//! and vector embedding. Compaction always goes first; embedding runs only when compaction had
//! nothing to do. Reindex keeps its own pool: `prepare_reindex` stops this worker and a commit
//! or abandoned reindex starts it again.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use core_embed::{Embedder, ModelRegistry, ModelSpec};
use core_index::lsm::{
    compaction::CompactionConfig,
    index_worker::IndexCommand,
    worker::{run_compaction_cycle, sleep_unless_stopped},
};
use slog::{Logger, error, info, warn};

use crate::{embed::EmbeddingJob, metrics::{BackgroundActivity, ShardStatsHandle}};

/// How long to wait before trying to load the embedding model again after a failure.
const MODEL_RETRY: Duration = Duration::from_secs(60);
/// How often to re-measure embedding progress while there is nothing to embed.
const MEASURE_EVERY: Duration = Duration::from_secs(60);
pub struct BackgroundWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<io::Result<()>>>,
}

impl BackgroundWorker {
    /// Returns None when there is nothing to run: compaction disabled and no vector fields.
    pub fn start(
        sender: Sender<IndexCommand>,
        compaction: Option<CompactionConfig>,
        interval: Duration,
        embedding: Option<EmbeddingJob>,
        models: Arc<ModelRegistry>,
        stats: ShardStatsHandle,
        log: Logger,
    ) -> Option<Self> {
        if compaction.is_none() && embedding.is_none() {
            return None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("shard-background".to_string())
            .spawn(move || {
                BackgroundLoop {
                    sender,
                    compaction,
                    interval,
                    embedding,
                    models,
                    stats,
                    embedder: None,
                    last_model_attempt: None,
                    last_measure: None,
                    log,
                }
                .run(&stop_thread)
            })
            .expect("failed to spawn shard background worker");
        Some(Self {
            stop,
            handle: Some(handle),
        })
    }

    /// Stops after the current step and waits for the thread.
    pub fn stop(mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .map_err(|_| io::Error::other("shard background worker panicked"))??;
        }
        Ok(())
    }

    /// Signals the thread to stop without waiting for it.
    pub fn stop_async(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.take();
    }
}

impl Drop for BackgroundWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct BackgroundLoop {
    sender: Sender<IndexCommand>,
    compaction: Option<CompactionConfig>,
    interval: Duration,
    embedding: Option<EmbeddingJob>,
    models: Arc<ModelRegistry>,
    stats: ShardStatsHandle,
    embedder: Option<Arc<Embedder>>,
    last_model_attempt: Option<Instant>,
    last_measure: Option<Instant>,
    log: Logger,
}

impl BackgroundLoop {
    fn run(mut self, stop: &AtomicBool) -> io::Result<()> {
        let result = self.run_loop(stop);
        self.stats.set_activity(BackgroundActivity::Idle);
        result
    }

    fn run_loop(&mut self, stop: &AtomicBool) -> io::Result<()> {
        while !stop.load(Ordering::Relaxed) {
            if let Some(config) = self.compaction {
                self.stats.set_activity(BackgroundActivity::Compacting);
                let started = Instant::now();
                let result = run_compaction_cycle(&self.sender, config, &self.log);
                self.stats.set_activity(BackgroundActivity::Idle);
                match result {
                    // A merge was installed; more may be due, so check again before anything else.
                    Ok(true) => {
                        self.stats.record_compaction(true, started.elapsed());
                        continue;
                    }
                    Ok(false) => {}
                    Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
                    Err(e) => {
                        self.stats.record_compaction(false, started.elapsed());
                        error!(self.log, "index compaction cycle failed; retrying after interval";
                            "error" => %e);
                        sleep_unless_stopped(stop, self.interval);
                        continue;
                    }
                }
            }

            match self.embed_once(stop) {
                // Did work: check compaction first, then come back.
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
                Err(e) => warn!(self.log, "embedding round failed"; "error" => %e),
            }

            sleep_unless_stopped(stop, self.interval);
        }
        Ok(())
    }

    /// One embedding round if there is a job and the model is free.
    /// Returns true when it did work.
    fn embed_once(&mut self, stop: &AtomicBool) -> io::Result<bool> {
        if self.embedding.is_none() {
            return Ok(false);
        }
        let Some(embedder) = self.embedder() else {
            return Ok(false);
        }; 
        let spec = embedder.spec();
        let vector_id = embedder.vector_id().to_string();
        let dims = embedder.spec().dims;
        if self.last_measure.is_none_or(|at| at.elapsed() >= MEASURE_EVERY) {
            self.measure(&vector_id, dims);
        }
        // Counting needs no model, so a shard waiting for the lease still reports progress.
        // Never wait for the model: another shard is embedding, and waiting here would stall
        // this shard's compaction.
        let Some(mut lease) = embedder.try_lease()? else {
            return Ok(false);
        };

        let round_started = Instant::now();
        self.stats.set_activity(BackgroundActivity::Embedding);
        let result = self
            .embedding
            .as_mut()
            .expect("checked above")
            .step(&mut lease, stop);
        self.stats.set_activity(BackgroundActivity::Idle);
        drop(lease);
        

        let did_work = match result? {
            None => false,
            Some(count) => {
                if count > 0 {
                    self.stats.add_embedded(count as u64, round_started);
                    info!(self.log, "embedded documents"; "count" => count);
                }
                true
            }
        };
        if did_work {
            self.measure(&vector_id,dims);
        }
        Ok(did_work)
    }

    fn measure(&mut self, vector_id: &str, dims: usize) {
        let job = self.embedding.as_ref().expect("measure is only called with a job");
        match job.measure(vector_id, dims) {
            Ok((total, done)) => self.stats.set_vector_progress(total, done),
            Err(e) => warn!(self.log, "measuring embedding progress failed"; "error" => %e),
        }
        self.last_measure = Some(Instant::now());
    }

    /// The shared model, loaded on first use; after a failure, retried at most once a minute.
    fn embedder(&mut self) -> Option<Arc<Embedder>> {
        if let Some(embedder) = &self.embedder {
            return Some(Arc::clone(embedder));
        }
        if self
            .last_model_attempt
            .is_some_and(|attempt| attempt.elapsed() < MODEL_RETRY)
        {
            return None;
        }
        self.last_model_attempt = Some(Instant::now());
        match self.models.default_embedder() {
            Ok(embedder) => {
                info!(self.log, "embedding model ready"; "model" => embedder.spec().id);
                self.embedder = Some(Arc::clone(&embedder));
                Some(embedder)
            }
            Err(e) => {
                warn!(self.log, "embedding model unavailable; retrying later"; "error" => %e);
                None
            }
        }
    }
}

