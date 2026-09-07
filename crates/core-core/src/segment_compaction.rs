use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

// core-core, e.g. shard_worker.rs or a new segment_compaction.rs in core-core
use core_storage::binary_store::run_segment_compaction;
use crossbeam_channel::Sender;

use crate::shard_worker::ShardCmd;

pub struct SegmentCompactionWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<io::Result<()>>>,
}

impl SegmentCompactionWorker {
    pub fn start(sender: Sender<ShardCmd>, dead_ratio_threshold: f64, interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                // eprintln!("[segcompact] tick, threshold={dead_ratio_threshold}");
                let (reply, rx) = std::sync::mpsc::channel();
                sender
                    .send(ShardCmd::PlanSegmentCompaction {
                        dead_ratio_threshold,
                        reply,
                    })
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "shard worker stopped")
                    })?;
                match rx.recv() {
                    Ok(Ok(Some(job))) => {
                        eprintln!("[segcompact] got job: {} segments", job.segment_ids.len());
                        match run_segment_compaction(job) {
                            Ok(completed) => {
                                let (ack, install_rx) = std::sync::mpsc::channel();
                                match sender.send(ShardCmd::InstallSegmentCompaction {
                                    completed,
                                    ack: Some(ack),
                                }) {
                                    Ok(()) => match install_rx.recv() {
                                        Ok(Ok(installed)) => {
                                            eprintln!("[segcompact] install result: {installed}")
                                        }
                                        Ok(Err(e)) => eprintln!("[segcompact] install error: {e}"),
                                        Err(_) => {
                                            eprintln!("[segcompact] install ack channel dropped")
                                        }
                                    },
                                    Err(_) => eprintln!(
                                        "[segcompact] shard worker stopped before install send"
                                    ),
                                }
                            }
                            Err(e) => eprintln!("[segcompact] run_segment_compaction failed: {e}"),
                        }
                    }
                    Ok(Ok(None)) => eprintln!("[segcompact] no candidates this tick"),
                    Ok(Err(e)) => eprintln!("[segcompact] plan error: {e}"),
                    Err(_) => eprintln!(
                        "[segcompact] plan reply channel dropped — shard worker likely stopped"
                    ),
                }
                thread::sleep(interval);
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
        if let Some(h) = self.handle.take() {
            h.join()
                .map_err(|_| io::Error::other("segment compaction worker panicked"))??;
        }
        Ok(())
    }
}
