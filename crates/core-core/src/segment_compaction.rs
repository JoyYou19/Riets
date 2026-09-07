use crate::shard_worker::ShardCmd;
use core_storage::binary_store::run_segment_compaction;
use crossbeam_channel::Sender;
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

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
                let (reply, rx) = std::sync::mpsc::channel();
                sender
                    .send(ShardCmd::PlanSegmentCompaction {
                        dead_ratio_threshold,
                        reply,
                    })
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "shard worker stopped")
                    })?;
                if let Ok(Ok(Some(job))) = rx.recv() {
                    if let Ok(completed) = run_segment_compaction(job) {
                        let (ack, install_rx) = std::sync::mpsc::channel();
                        if sender
                            .send(ShardCmd::InstallSegmentCompaction {
                                completed,
                                ack: Some(ack),
                            })
                            .is_ok()
                        {
                            let _ = install_rx.recv();
                        }
                    }
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
