use std::{
    io,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread::{self},
    time::{Duration, Instant},
};

use core_timing::timed;

use crate::{background::background_pool, lsm::{
    compaction::{
        CompactionConfig, CompactionJob, CompletedCompaction, compact_segments_streaming,
        compact_vectors,
    },
    index_worker::IndexCommand,
}};


const STOP_POLL: Duration = Duration::from_millis(100);


/// Plans, runs and installs at most one compaction.
/// Returns Ok(true) if a job was installed, Ok(false) if there was nothing to do.
pub fn run_compaction_cycle(
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
    let completed = match background_pool().install(|| run_compaction_job(job)) {
        Ok(completed) => completed,
        Err(err) => {
            if let Err(remove_err) = std::fs::remove_file(&output_path) {
                if remove_err.kind() != io::ErrorKind::NotFound {
                    slog::warn!(log, "failed to remove partial compaction output";
                        "path" => %output_path.display(),
                        "error" => %remove_err);
                }
            }
            return Err(io::Error::new(err.kind(), format!("{}: {}", output_path.display(), err)));
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

pub fn sleep_unless_stopped(stop: &AtomicBool, total: Duration) {
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
    let vector_xpaths = compact_vectors(&job.vectors, &job.deleted, &job.output_path);
    Ok(CompletedCompaction {
        job_id: job.job_id,
        selected: job.selected,
        output_path: job.output_path,
        delete_generation: job.delete_generation,
        vector_xpaths,
    })
}
