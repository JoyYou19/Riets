use rayon::prelude::*;
use std::{
    io,
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
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
        ack: Option<Acknowledgement>,
    },
    Flush {
        ack: Option<Acknowledgement>,
    },
    MaybeCompact {
        config: CompactionConfig,
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
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "index worker already joined"))?;
        handle
            .join()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "index worker panicked"))?
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

    pub fn add_segment(&self, segment: ImmutableSegment) -> io::Result<()> {
        self.send(IndexCommand::AddSegment { segment, ack: None })
    }

    pub fn flush(&self) -> io::Result<()> {
        self.send(IndexCommand::Flush { ack: None })
    }

    pub fn maybe_compact(&self, config: CompactionConfig) -> io::Result<()> {
        self.send(IndexCommand::MaybeCompact { config, ack: None })
    }

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
    pub fn add_indexed_document_wait(&self, document: IndexedDocument) -> io::Result<()> {
        let (ack, rx) = mpsc::channel();

        self.send(IndexCommand::AddIndexedDocument {
            document,
            ack: Some(ack),
        })?;

        wait_for_acknowledgement(rx)
    }

    pub fn flush_wait(&self) -> io::Result<()> {
        let (ack, rx) = mpsc::channel();

        self.send(IndexCommand::Flush { ack: Some(ack) })?;

        wait_for_acknowledgement(rx)
    }

    pub fn maybe_compact_wait(&self, config: CompactionConfig) -> io::Result<()> {
        let (ack, rx) = mpsc::channel();

        self.send(IndexCommand::MaybeCompact {
            config,
            ack: Some(ack),
        })?;

        wait_for_acknowledgement(rx)
    }

    pub fn delete_document_wait(&self, doc_id: DocId) -> io::Result<()> {
        let (ack, rx) = mpsc::channel();

        self.send(IndexCommand::DeleteDocument {
            doc_id,
            ack: Some(ack),
        })?;

        wait_for_acknowledgement(rx)
    }

    pub fn add_segment_wait(&self, segment: ImmutableSegment) -> io::Result<()> {
        let (ack, rx) = mpsc::channel();

        self.send(IndexCommand::AddSegment {
            segment,
            ack: Some(ack),
        })?;

        wait_for_acknowledgement(rx)
    }

    pub fn install_compaction_wait(&self, completed: CompletedCompaction) -> io::Result<()> {
        let (ack, rx) = mpsc::channel();

        self.send(IndexCommand::InstallCompaction {
            completed,
            ack: Some(ack),
        })?;

        wait_for_acknowledgement(rx)
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
}

fn run_index_worker(
    mut index: LsmIndex,
    analyzer: Analyzer,
    shared: SharedIndexSnapshot,
    receiver: Receiver<IndexCommand>,
) -> io::Result<LsmIndex> {
    while let Ok(command) = receiver.recv() {
        match command {
            IndexCommand::AddIndexedDocument { document, ack } => {
                let result = index.add_indexed_document(&analyzer, &document).map(|_| {
                    shared.publish(index.snapshot());
                });

                send_acknowledgement(ack, result)?
            }
            IndexCommand::DeleteDocument { doc_id, ack } => {
                let result = index.delete_document(doc_id).map(|_| {
                    shared.publish(index.snapshot());
                });
                send_acknowledgement(ack, result)?
            }
            IndexCommand::AddSegment { segment, ack } => {
                let result = index.add_immutable_segment(segment).map(|_| {
                    shared.publish(index.snapshot());
                });
                send_acknowledgement(ack, result)?
            }
            IndexCommand::MaybeCompact { config, ack } => {
                let result = index.maybe_compact(config).map(|_| {
                    shared.publish(index.snapshot());
                });
                send_acknowledgement(ack, result)?
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
                let result = index.install_compaction(completed).map(|installed| {
                    if installed {
                        shared.publish(index.snapshot());
                    }
                });

                send_acknowledgement(ack, result)?
            }
            IndexCommand::SegmentCount { reply } => {
                reply.send(Ok(index.segment_count())).map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "segment count receiver dropped")
                })?;
            }
            IndexCommand::Flush { ack } => {
                let result = index.flush().map(|_| {
                    shared.publish(index.snapshot());
                });
                send_acknowledgement(ack, result)?
            }
            IndexCommand::Shutdown => {
                index.flush()?;
                shared.publish(index.snapshot());
                return Ok(index);
            }
        }
    }

    index.flush()?;
    Ok(index)
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

pub fn index_batches_parallel(
    worker: &IndexWorker,
    analyzer: Analyzer,
    batches: Vec<Vec<IndexedDocument>>,
) -> io::Result<()> {
    let segments: Vec<ImmutableSegment> = batches
        .into_par_iter()
        .map(|batch| build_segment_batch(&analyzer, batch))
        .collect();

    for segment in segments {
        worker.add_segment_wait(segment)?;
    }

    Ok(())
}

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

pub fn build_segments_parallel(
    analyzer: Analyzer,
    batches: Vec<Vec<IndexedDocument>>,
) -> Vec<ImmutableSegment> {
    batches
        .into_par_iter()
        .map(|batch| build_segment_batch(&analyzer, batch))
        .collect()
}
