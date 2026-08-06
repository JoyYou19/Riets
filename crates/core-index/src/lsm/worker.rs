use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::{
    disk::{reader::DiskSegment, writer::write_segment},
    lsm::{
        compact_segments,
        compaction::{CompactionConfig, CompactionJob, CompletedCompaction},
        index_worker::IndexCommand,
    },
    segment::SegmentHandle,
};

// Whenever started will wait for an interval before
// checking if an index command has been posted,
// if a planned compaction has been scheduled, will run a
// compaction job with the provided segments to compact
pub struct CompactionWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<io::Result<()>>>,
}

impl CompactionWorker {
    pub fn start(
        sender: Sender<IndexCommand>,
        config: CompactionConfig,
        interval: Duration,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();

        let handle = thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                let (reply, rx) = std::sync::mpsc::channel();

                sender
                    .send(IndexCommand::PlanCompaction { config, reply })
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "index worker stopped")
                    })?;

                if let Some(job) = rx.recv().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "index worker dropped compaction plan reply",
                    )
                })?? {
                    tracing::info!(
                        job_id = job.job_id,
                        segments = job.selected.len(),
                        "compaction planned"
                    );

                    let started = std::time::Instant::now();
                    let completed = run_compaction_job(job)?;

                    tracing::info!(
                        job_id = completed.job_id,
                        elapsed_ms = started.elapsed().as_millis(),
                        "compaction finished"
                    );

                    let (ack, install_rx) = std::sync::mpsc::channel();

                    sender
                        .send(IndexCommand::InstallCompaction {
                            completed,
                            ack: Some(ack),
                        })
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::BrokenPipe, "index worker stopped")
                        })?;

                    install_rx.recv().map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "index worker dropped install acknowledgement",
                        )
                    })??;
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

        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .map_err(|_| io::Error::other("compaction worker panicked"))??;
        }

        Ok(())
    }
}

fn run_compaction_job(job: CompactionJob) -> io::Result<CompletedCompaction> {
    let mut segments = Vec::new();

    for handle in &job.selected {
        match handle {
            SegmentHandle::Disk(path) => {
                let disk = DiskSegment::open(path)?;
                segments.push(disk.to_immutable_segment());
            }
            SegmentHandle::Memory(segment) => {
                segments.push((**segment).clone());
            }
        }
    }

    let compacted = compact_segments(&segments, &job.deleted);
    write_segment(&job.output_path, &compacted)?;

    Ok(CompletedCompaction {
        job_id: job.job_id,
        selected: job.selected,
        output_path: job.output_path,
    })
}
