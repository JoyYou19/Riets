use std::{
    io,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use core_timing::timed;

use crate::lsm::{
    //compact_segments,
    compaction::{
        CompactionConfig, CompactionJob, CompletedCompaction, compact_segments_streaming,
    },
    index_worker::IndexCommand,
};

const MAX_CONCURRENT_INDEX_COMPACTIONS: usize = 8;
const STOP_POLL: Duration = Duration::from_millis(100);

static INDEX_COMPACTION_SLOTS: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

struct CompactionPermit;

impl CompactionPermit {
    fn acquire() -> Self {
        let (lock, available) = &INDEX_COMPACTION_SLOTS;
        let mut running = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        while *running >= MAX_CONCURRENT_INDEX_COMPACTIONS {
            running = available
                .wait(running)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *running += 1;
        CompactionPermit
    }
}

impl Drop for CompactionPermit {
    fn drop(&mut self) {
        let (lock, available) = &INDEX_COMPACTION_SLOTS;
        let mut running = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *running -= 1;
        available.notify_one();
    }
}

pub struct CompactionWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<io::Result<()>>>,
}

impl CompactionWorker {
    pub fn start(
        sender: Sender<IndexCommand>,
        config: CompactionConfig,
        interval: Duration,
        log: slog::Logger,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();

        let handle = thread::spawn(move || -> io::Result<()> {
            while !stop_thread.load(Ordering::Relaxed) {
                match run_cycle(&sender, config, &log) {
                    Ok(true) => {
                        continue;
                    }
                    Ok(false) => {}
                    Err(err) if err.kind() == io::ErrorKind::BrokenPipe => {
                        return Ok(());
                    }
                    Err(err) => {
                        slog::error!(log, "index compaction cycle failed; retrying after interval";
                        "error" => %err);
                    }
                }
                sleep_unless_stopped(&stop_thread, interval);
            }
            Ok(())
        });

        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub fn stop(mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .map_err(|_| io::Error::other("compaction worker panicked"))??;
        }
        Ok(())
    }

    // Stops the worker without waiting; the thread exits once it notices the flag.
    pub fn stop_async(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            drop(handle);
        }
    }
}

/// Plans, runs and installs at most one compaction.
/// Returns Ok(true) if a job was installed, Ok(false) if there was nothing to do.
fn run_cycle(
    sender: &Sender<IndexCommand>,
    config: CompactionConfig,
    log: &slog::Logger,
) -> io::Result<bool> {
    let (reply, plan_rx) = std::sync::mpsc::channel();
    sender
        .send(IndexCommand::PlanCompaction { config, reply })
        .map_err(|_| broken_pipe("index worker stopped"))?;

    let Some(job) = plan_rx
        .recv()
        .map_err(|_| broken_pipe("index worker dropped compaction plan reply"))??
    else {
        return Ok(false);
    };
    // slet shard_log=slog::Logger;
    let output_path = job.output_path.clone();
    let completed = {
        let _permit = CompactionPermit::acquire();
        match run_compaction_job(job) {
            Ok(completed) => completed,
            Err(err) => {
                if let Err(remove_err) = std::fs::remove_file(&output_path) {
                    if remove_err.kind() != io::ErrorKind::NotFound {
                        slog::warn!(log, "failed to remove partial compaction output";
                     "path" => %output_path.display(),
                     "error" => %remove_err);
                    }
                }
                return Err(io::Error::new(
                    err.kind(),
                    format!("{}: {}", output_path.display(), err),
                ));
            }
        }
    };

    let (ack, install_rx) = std::sync::mpsc::channel();
    sender
        .send(IndexCommand::InstallCompaction {
            completed,
            ack: Some(ack),
        })
        .map_err(|_| broken_pipe("index worker stopped"))?;
    let _ = install_rx
        .recv()
        .map_err(|_| broken_pipe("index worker dropped install acknowledgement"))??;

    Ok(true)
}

fn sleep_unless_stopped(stop: &AtomicBool, total: Duration) {
    let deadline = Instant::now() + total;
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep(STOP_POLL.min(deadline - now));
    }
}

fn broken_pipe(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, message)
}

#[timed(compaction)]
fn run_compaction_job(job: CompactionJob) -> io::Result<CompletedCompaction> {
    compact_segments_streaming(&job.selected, &job.deleted, &job.output_path)?;
    Ok(CompletedCompaction {
        job_id: job.job_id,
        selected: job.selected,
        output_path: job.output_path,
        delete_generation: job.delete_generation,
    })
}
