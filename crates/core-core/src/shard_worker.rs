use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use core_protocol::command_reponse_definitions::{
    LookupCommand, LookupResponse, RetrieveCommand, RetrieveResponse,
};
use core_storage::document_store::StoredDocument;
use crossbeam_channel::{Receiver, Sender, bounded};

use crate::shard_db::ShardDb;
use crate::{DatabaseOptions, options};
use core_index::document::IndexPolicy;
use core_index::lsm::index_worker::ReindexProgress;
use core_index::types::ShardId;
use core_protocol::errors::CorelamoError;
use core_query::Query; // QueryPlan prom
use core_storage::search_database::{DocumentInput, InsertReport, SearchDocumentHit};

pub enum ShardCmd {
    Insert {
        inputs: Vec<DocumentInput>,
        resp: Sender<Result<InsertReport, CorelamoError>>,
    },
    Search {
        query: Arc<Query>,
        k: usize,
        resp: Sender<Result<Vec<SearchDocumentHit>, CorelamoError>>,
    },

    Retrieve {
        ids: Vec<String>,
        resp: Sender<Result<Vec<(String, Option<StoredDocument>)>, CorelamoError>>,
    },

    Lookup {
        command: LookupCommand,
        resp: Sender<Result<LookupResponse, CorelamoError>>,
    },

    Flush {
        resp: Sender<Result<(), CorelamoError>>,
    },
    SetPolicy {
        policy: IndexPolicy,
        resp: Sender<Result<(), CorelamoError>>,
    },
    SetConfig {
        options: DatabaseOptions,
        resp: Sender<Result<(), CorelamoError>>,
    },
    Clear {
        resp: Sender<Result<(), CorelamoError>>,
    },

    Start {
        resp: Sender<Result<(), CorelamoError>>,
    },
    Stop {
        resp: Sender<Result<(), CorelamoError>>,
    },

    DocCount {
        resp: Sender<usize>,
    },
    Shutdown {
        resp: Sender<Result<(), CorelamoError>>,
    },
}

#[derive(Clone)]
pub struct ShardHandle {
    id: ShardId,
    tx: Sender<ShardCmd>,
    alive: Arc<AtomicBool>,
    progress: Arc<ReindexProgress>,
}

impl ShardHandle {
    pub fn id(&self) -> ShardId {
        self.id
    }
    pub fn progress(&self) -> &Arc<ReindexProgress> {
        &self.progress
    }
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
    pub fn queued(&self) -> usize {
        self.tx.len()
    }

    pub fn clear(&self) -> Result<(), CorelamoError> {
        self.call(|resp| ShardCmd::Clear { resp })?
    }

    /// Fire-and-forget send for manager fan-out; caller keeps the response rx.
    /// On failure the command comes back so the caller can recover its payload.
    pub(crate) fn send_raw(&self, cmd: ShardCmd) -> Result<(), (CorelamoError, ShardCmd)> {
        self.tx.send(cmd).map_err(|e| (self.dead(), e.0))
    }

    fn call<T>(&self, make: impl FnOnce(Sender<T>) -> ShardCmd) -> Result<T, CorelamoError> {
        let (rtx, rrx) = bounded(1);
        self.tx.send(make(rtx)).map_err(|_| self.dead())?;
        rrx.recv().map_err(|_| self.dead())
    }

    fn dead(&self) -> CorelamoError {
        CorelamoError::Internal(format!("shard {} is not running", self.id))
    }

    pub fn insert(&self, inputs: Vec<DocumentInput>) -> Result<InsertReport, CorelamoError> {
        self.call(|resp| ShardCmd::Insert { inputs, resp })?
    }

    pub fn flush(&self) -> Result<(), CorelamoError> {
        self.call(|resp| ShardCmd::Flush { resp })?
    }

    pub fn set_policy(&self, policy: IndexPolicy) -> Result<(), CorelamoError> {
        self.call(|resp| ShardCmd::SetPolicy { policy, resp })?
    }

    pub fn start(&self) -> Result<(), CorelamoError> {
        self.call(|resp| ShardCmd::Start { resp })?
    }
    pub fn stop(&self) -> Result<(), CorelamoError> {
        self.call(|resp| ShardCmd::Stop { resp })?
    }

    pub fn set_config(&self, options: DatabaseOptions) -> Result<(), CorelamoError> {
        self.call(|resp| ShardCmd::SetConfig { options, resp })?
    }

    pub fn document_count(&self) -> Result<usize, CorelamoError> {
        self.call(|resp| ShardCmd::DocCount { resp })
    }

    pub fn shutdown(&self) -> Result<(), CorelamoError> {
        self.call(|resp| ShardCmd::Shutdown { resp })?
    }
}

/// Flips `alive` to false on any exit path, including panic unwind.
struct AliveGuard(Arc<AtomicBool>);
impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub fn spawn(
    mut shard: ShardDb,
    queue_depth: usize,
) -> Result<(ShardHandle, JoinHandle<()>), CorelamoError> {
    let id = shard.shard_id();
    let progress = shard.progress();
    let alive = Arc::new(AtomicBool::new(true));

    let (tx, rx) = bounded(queue_depth.max(1));
    let (boot_tx, boot_rx) = bounded(1);

    let alive_worker = alive.clone();
    let join = thread::Builder::new()
        .name(format!("shard-{}", id))
        .spawn(move || {
            let _guard = AliveGuard(alive_worker);
            let started = shard.start();
            let ok = started.is_ok();
            let _ = boot_tx.send(started);
            if ok {
                run(shard, rx);
            }
        })
        .map_err(|e| CorelamoError::Internal(format!("failed to spawn shard {id}: {e}")))?;

    boot_rx
        .recv()
        .map_err(|_| CorelamoError::Internal(format!("shard {id} thread died during start")))??;

    Ok((
        ShardHandle {
            id,
            tx,
            alive,
            progress,
        },
        join,
    ))
}

fn run(mut shard: ShardDb, rx: Receiver<ShardCmd>) {
    const MAX_BATCH: usize = 32;

    let mut batch: Vec<ShardCmd> = Vec::with_capacity(MAX_BATCH);

    while let Ok(first) = rx.recv() {
        batch.push(first);
        batch.extend(rx.try_iter().take(MAX_BATCH - 1));

        for cmd in batch.drain(..) {
            match cmd {
                ShardCmd::Insert { inputs, resp } => {
                    let _ = resp.send(shard.insert(inputs));
                }
                ShardCmd::Search { query, k, resp } => {
                    let _ = resp.send(shard.search(&query, k));
                }

                ShardCmd::Lookup { command, resp } => {
                    let _ = resp.send(shard.lookup(&command));
                }

                ShardCmd::Flush { resp } => {
                    let _ = resp.send(shard.flush());
                }
                ShardCmd::SetConfig { options, resp } => {
                    let _ = resp.send(shard.set_options(options));
                }
                ShardCmd::SetPolicy { policy, resp } => {
                    let _ = resp.send(shard.set_policy(policy));
                }
                ShardCmd::DocCount { resp } => {
                    let _ = resp.send(shard.document_count());
                }

                ShardCmd::Start { resp } => {
                    let _ = resp.send(shard.start());
                }
                ShardCmd::Stop { resp } => {
                    let _ = resp.send(shard.stop());
                }

                ShardCmd::Clear { resp } => {
                    let _ = resp.send(shard.clear());
                }
                ShardCmd::Shutdown { resp } => {
                    let _ = resp.send(shard.stop());
                    return;
                }
                ShardCmd::Retrieve { ids, resp } => {
                    let _ = resp.send(shard.get_document(&ids));
                }
            }
        }
    }

    // All senders dropped without an explicit Shutdown.
    let _ = shard.stop();
}
