use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use core_index::analyzer::Analyzer;
use core_index::lsm::snapshot::SharedIndexSnapshot;
use core_protocol::command_reponse_definitions::{
    LookupCommand, LookupResponse, RetrieveCommand, RetrieveResponse,
};
use core_storage::document_store::StoredDocument;
use crossbeam_channel::{Receiver, Sender, bounded};
use dashmap::DashMap;
use indexmap::IndexMap;

use crate::reindex::{CompletedShardReindex, ReindexParams};
use crate::shard_db::ShardDb;
use crate::{DatabaseOptions, options};
use core_index::document::IndexPolicy;
use core_index::lsm::index_worker::ReindexProgress;
use core_index::types::{DocId, ShardId};
use core_protocol::errors::CorelamoError;
use core_query::{Query, QueryExecutor, SearchHit}; // QueryPlan prom
use core_storage::search_database::{
    DeleteReport, DocumentInput, InsertReport, ReplaceReport, SearchDocumentHit, visible_fields,
};

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
    IsRunning {
        resp: Sender<bool>,
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

    Upsert {
        inputs: Vec<DocumentInput>,
        resp: Sender<Result<InsertReport, CorelamoError>>,
    },

    Replace {
        inputs: Vec<DocumentInput>,
        resp: Sender<Result<ReplaceReport, CorelamoError>>,
    },
    Delete {
        ids: Vec<String>,
        resp: Sender<Result<DeleteReport, CorelamoError>>,
    },
    PrepareReindex {
        
        resp:Sender<Result<ReindexParams,CorelamoError>>,
    },
    CommitReindex{
        done:CompletedShardReindex,
        resp: Sender<Result<(), CorelamoError>>
    },
    Start {
        resp: Sender<Result<(), CorelamoError>>,
    },
    Stop {
        resp: Sender<Result<(), CorelamoError>>,
    },

    GetLogs {
        date: Option<String>,
        resp: Sender<Result<String, CorelamoError>>,
    },
    ClearLogs {
        resp: Sender<Result<(), CorelamoError>>,
    },

    DocCount {
        resp: Sender<usize>,
    },

    ResolveHits {
        hits: Vec<SearchHit>,
        resp: Sender<Result<Vec<SearchDocumentHit>, CorelamoError>>,
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
    is_running: Arc<AtomicBool>,
    is_clearing: Arc<AtomicBool>,
    shared_snapshot: SharedIndexSnapshot,

    shared_docs: Arc<DashMap<String, StoredDocument>>,
    shared_internal_to_external: Arc<DashMap<DocId, String>>,
}

impl ShardHandle {
    fn ensure_readable(&self) -> Result<(), CorelamoError> {
        if !self.is_running() {
            return Err(CorelamoError::DatabaseNotRunning(format!(
                "shard {} is not running",
                self.id
            )));
        }
        if self.is_clearing() {
            return Err(CorelamoError::Busy(format!(
                "shard {} is clearing",
                self.id
            )));
        }
        Ok(())
    }

    pub fn get_document_direct(
        &self,
        ids: &[String],
    ) -> Result<Vec<(String, Option<StoredDocument>)>, CorelamoError> {
        self.ensure_readable()?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let doc = self.shared_docs.get(id).map(|r| r.value().clone());
            out.push((id.clone(), doc));
        }
        Ok(out)
    }

    pub fn lookup_direct(
        &self,
        ids: &[String],
        return_fields: Option<&IndexMap<String, bool>>,
        policy: &IndexPolicy,
    ) -> Result<LookupResponse, CorelamoError> {
        self.ensure_readable()?;
        let mut found = Vec::new();
        let mut not_found = Vec::new();
        for id in ids {
            match self.shared_docs.get(id) {
                Some(entry) => found.push((
                    entry.value().external_id.clone(),
                    visible_fields(&entry.value().fields, policy, return_fields),
                )),
                None => not_found.push(id.clone()),
            }
        }
        LookupResponse::from_hits(found, not_found)
            .map_err(io::Error::other)
            .map_err(CorelamoError::from)
    }

    /// Turns already-ranked SearchHits into full SearchDocumentHits, reading
    /// the store directly. Replaces the old ShardCmd::ResolveHits round trip —
    /// this closes the phase-1 gap.
    pub fn resolve_hits_direct(
        &self,
        hits: Vec<SearchHit>,
        policy: &IndexPolicy,
    ) -> Result<Vec<SearchDocumentHit>, CorelamoError> {
        self.ensure_readable()?;
        let mut results = Vec::with_capacity(hits.len());
        for hit in hits {
            let Some(external_id) = self
                .shared_internal_to_external
                .get(&hit.doc_id)
                .map(|r| r.value().clone())
            else {
                continue;
            };
            let Some(doc) = self.shared_docs.get(&external_id) else {
                continue;
            };
            results.push(SearchDocumentHit {
                external_id: doc.external_id.clone(),
                internal_id: doc.internal_id,
                score: hit.score,
                fields: visible_fields(&doc.fields, policy, None),
            });
        }
        Ok(results)
    }

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

    pub fn is_clearing(&self) -> bool {
        self.is_clearing.load(Ordering::Acquire)
    }
    pub fn is_readable(&self) -> bool {
        self.is_running() && !self.is_clearing()
    }

    pub fn clear(&self) -> Result<(), CorelamoError> {
        self.call(|resp| ShardCmd::Clear { resp })?
    }

    pub fn rank_top_k(
        &self,
        query: &Query,
        k: usize,
        analyzer: &Analyzer,
        policy: &IndexPolicy,
    ) -> Vec<SearchHit> {
        let snapshot = self.shared_snapshot.get();
        let executor = QueryExecutor::new(&*snapshot, analyzer);
        let xpaths: Vec<_> = policy.searchable_xpaths().collect();
        executor.search_all_xpaths_top_k(query, xpaths, k)
    }

    pub fn is_running(&self) -> bool {
        self.is_running.load(Ordering::Acquire)
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

    pub fn upsert(&self, inputs: Vec<DocumentInput>) -> Result<InsertReport, CorelamoError> {
        self.call(|resp| ShardCmd::Upsert { inputs, resp })?
    }
    pub fn replace(&self, inputs: Vec<DocumentInput>) -> Result<ReplaceReport, CorelamoError> {
        self.call(|resp| ShardCmd::Replace { inputs, resp })?
    }
    pub fn delete(&self, ids: Vec<String>) -> Result<DeleteReport, CorelamoError> {
        self.call(|resp| ShardCmd::Delete { ids, resp })?
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

    pub fn get_logs(&self, date: Option<String>) -> Result<String, CorelamoError> {
        self.call(|resp| ShardCmd::GetLogs { date, resp })?
    }

    pub fn clear_logs(&self) -> Result<(), CorelamoError> {
        self.call(|resp| ShardCmd::ClearLogs { resp })?
    }

    pub fn document_count(&self) -> Result<usize, CorelamoError> {
        self.call(|resp| ShardCmd::DocCount { resp })
    }

    pub fn shutdown(&self) -> Result<(), CorelamoError> {
        self.call(|resp| ShardCmd::Shutdown { resp })?
    }
    //reindex
    pub(crate) fn command_sender(&self) -> Sender<ShardCmd> {
    self.tx.clone()
    }
}

/// Flips `alive` to false on any exit path, including panic unwind.
struct AliveGuard(Arc<AtomicBool>);
impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

// spawn() — create it alongside is_running, pass into run(), include in the literal
pub fn spawn(
    mut shard: ShardDb,
    queue_depth: usize,
) -> Result<(ShardHandle, JoinHandle<()>), CorelamoError> {
    let id = shard.shard_id();
    let progress = shard.progress();
    let alive = Arc::new(AtomicBool::new(true));
    let is_running = Arc::new(AtomicBool::new(false));
    let is_clearing = Arc::new(AtomicBool::new(false));
    let shared_snapshot = shard.shared_snapshot();
    let (shared_docs, shared_internal_to_external) = shard.shared_store_maps(); // NEW

    let (tx, rx) = bounded(queue_depth.max(1));
    let (boot_tx, boot_rx) = bounded(1);
    let job_tx= tx.clone();
    let alive_worker = alive.clone();
    let is_running_worker = is_running.clone();
    let is_clearing_worker = is_clearing.clone();
    let join = thread::Builder::new()
        .name(format!("shard-{}", id))
        .spawn(move || {
            let _guard = AliveGuard(alive_worker);
            let started = shard.start();
            let ok = started.is_ok();
            is_running_worker.store(ok, Ordering::Release);
            let _ = boot_tx.send(started);
            
            if ok {
                run(shard, rx, is_running_worker, is_clearing_worker);
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
            is_running,
            is_clearing,
            shared_snapshot,
            shared_docs,                 // NEW
            shared_internal_to_external, // NEW
        },
        join,
    ))
}
fn run(
    mut shard: ShardDb,
    rx: Receiver<ShardCmd>,
    is_running: Arc<AtomicBool>,
    is_clearing: Arc<AtomicBool>,
) {
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
                ShardCmd::Upsert { inputs, resp } => {
                    let _ = resp.send(shard.upsert(inputs));
                }
                ShardCmd::Replace { inputs, resp } => {
                    let _ = resp.send(shard.replace(inputs));
                }
                ShardCmd::Delete { ids, resp } => {
                    let _ = resp.send(shard.delete(ids));
                }
                ShardCmd::PrepareReindex{ resp} =>{
                    let _ = resp.send(shard.prepare_reindex());
                }
                ShardCmd::CommitReindex { done, resp } =>{
                    let _ = resp.send(shard.commit_reindex(done));
                }
                ShardCmd::IsRunning { resp } => {
                    let _ = resp.send(shard.is_running());
                }

                ShardCmd::ResolveHits { hits, resp } => {
                    let _ = resp.send(shard.resolve_hits(hits));
                ShardCmd::Start { resp } => {
                    let _ = resp.send(shard.start());
                }
                ShardCmd::Stop { resp } => {
                    let _ = resp.send(shard.stop());
                }

                ShardCmd::Clear { resp } => {
                    is_clearing.store(true, Ordering::Release);
                    let result = shard.clear();
                    is_clearing.store(false, Ordering::Release);
                    let _ = resp.send(result);
                }

                ShardCmd::GetLogs { date, resp } => {
                    let _ = resp.send(shard.get_logs(date));
                }

                ShardCmd::ClearLogs { resp } => {
                    let _ = resp.send(shard.clear_logs());
                }

                ShardCmd::Start { resp } => {
                    let result = shard.start();
                    is_running.store(result.is_ok(), Ordering::Release);
                    let _ = resp.send(result);
                }
                ShardCmd::Stop { resp } => {
                    let result = shard.stop();
                    is_running.store(false, Ordering::Release);
                    let _ = resp.send(result);
                }

                ShardCmd::Shutdown { resp } => {
                    let result = shard.stop();
                    is_running.store(false, Ordering::Release);
                    let _ = resp.send(result);
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
