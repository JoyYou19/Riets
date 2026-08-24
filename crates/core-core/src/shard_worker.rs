use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use core_backup::backup::BackupManifest;
use core_index::analyzer::Analyzer;
use core_protocol::command_reponse_definitions::LookupResponse;
use core_storage::document_store::StoredDocument;
use crossbeam_channel::{Receiver, Sender, bounded};
use indexmap::IndexMap;
use tokio::sync::oneshot;

use crate::DatabaseOptions;
use crate::reindex::{CompletedShardReindex, ReindexParams};
use crate::shard_db::ShardDb;
use crate::shared_state::SharedShardState;

use core_index::document::IndexPolicy;
use core_index::lsm::index_worker::ReindexProgress;
use core_index::types::ShardId;
use core_protocol::errors::CorelamoError;
use core_query::{Query, QueryExecutor, SearchHit};
use core_storage::search_database::{
    DeleteReport, DocumentInput, InsertReport, ReplaceReport, SearchDocumentHit, visible_fields,
};

//insane portno kur dazaam komandam ir crossbeam_channel dazam ir oneshot
pub enum ShardCmd {
    Insert {
        user: String,
        inputs: Vec<DocumentInput>,
        resp: oneshot::Sender<Result<InsertReport, CorelamoError>>,
    },
    Flush {
        resp: oneshot::Sender<Result<(), CorelamoError>>,
    },
    SetPolicy {
        user: String,
        policy: IndexPolicy,
        resp: oneshot::Sender<Result<(), CorelamoError>>,
    },
    SetConfig {
        user: String,
        options: DatabaseOptions,
        resp: oneshot::Sender<Result<(), CorelamoError>>,
    },
    Clear {
        resp: oneshot::Sender<Result<(), CorelamoError>>,
    },
    Upsert {
        user: String,
        inputs: Vec<DocumentInput>,
        resp: oneshot::Sender<Result<InsertReport, CorelamoError>>,
    },
    Replace {
        user: String,
        inputs: Vec<DocumentInput>,
        resp: oneshot::Sender<Result<ReplaceReport, CorelamoError>>,
    },
    Delete {
        user: String,
        ids: Vec<String>,
        resp: oneshot::Sender<Result<DeleteReport, CorelamoError>>,
    },
    PrepareReindex {
        resp: Sender<Result<ReindexParams, CorelamoError>>,
    },
    CommitReindex {
        done: CompletedShardReindex,
        resp: Sender<Result<(), CorelamoError>>,
    },
    Start {
        resp: oneshot::Sender<Result<(), CorelamoError>>,
    },
    Stop {
        resp: oneshot::Sender<Result<(), CorelamoError>>,
    },
    Shutdown {
        resp: Sender<Result<(), CorelamoError>>,
    },
    BackupFull {
        shard_backup_path: PathBuf,
        backup_id: String,
        resp: oneshot::Sender<Result<BackupManifest, CorelamoError>>,
    },
    BackupIncremental {
        shard_backup_path: PathBuf,
        backup_id: String,
        resp: oneshot::Sender<Result<Option<BackupManifest>, CorelamoError>>,
    },
    ListBackups {
        resp: oneshot::Sender<Result<Vec<BackupManifest>, CorelamoError>>,
    },
    Restore {
        user: String,
        backup_id: String,
        resp: oneshot::Sender<Result<(), CorelamoError>>,
    },
}
#[derive(Clone)]
pub struct ShardHandle {
    id: ShardId,
    tx: Sender<ShardCmd>,
    alive: Arc<AtomicBool>,
    progress: Arc<ReindexProgress>,
    analyzer: Analyzer,
    shared: Arc<SharedShardState>,
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
        if self.is_restoring() {
            return Err(CorelamoError::Busy(format!(
                "shard {} is restoring from backup",
                self.id
            )));
        }
        Ok(())
    }

    pub fn id(&self) -> ShardId {
        self.id
    }

    pub fn is_restoring(&self) -> bool {
        self.shared.is_restoring.load(Ordering::Acquire)
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

    pub fn is_running(&self) -> bool {
        self.shared.is_running.load(Ordering::Acquire)
    }

    pub fn is_clearing(&self) -> bool {
        self.shared.is_clearing.load(Ordering::Acquire)
    }

    // pub fn rank_top_k(&self, query: &Query, k: usize, policy: &IndexPolicy) -> Vec<SearchHit> {
    //     let snapshot = self.shared.snapshot.get();
    //     let executor = QueryExecutor::new(&*snapshot, &self.analyzer);
    //     let xpaths: Vec<_> = policy.searchable_xpaths().collect();
    //     executor.search_all_xpaths_top_k(query, xpaths, k)
    // }
    //

    pub fn rank_top_k(
        &self,
        query: Option<&Query>,
        filters: Option<&HashMap<String, String>>,
        k: usize,
        policy: &IndexPolicy,
    ) -> Result<Vec<SearchHit>, CorelamoError> {
        let snapshot = self.shared.snapshot.get();
        let executor = QueryExecutor::new(&*snapshot, &self.analyzer);

        let restrict = match filters {
            Some(f) => executor.resolve_filters(f, policy)?,
            None => None,
        };

        let xpaths: Vec<_> = policy.searchable_xpaths().collect();
        Ok(executor.search_all_xpaths_top_k_restricted(query, xpaths, k, restrict.as_ref()))
    }

    pub fn get_document_direct(
        &self,
        ids: &[String],
    ) -> Result<Vec<(String, Option<StoredDocument>)>, CorelamoError> {
        self.ensure_readable()?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let doc = self.shared.docs.get(id).map(|r| r.value().clone());
            out.push((id.clone(), doc));
        }
        Ok(out)
    }

    pub fn document_count_direct(&self) -> usize {
        self.shared.docs.len()
    }

    pub fn get_logs_direct(&self, date: Option<String>) -> Result<String, CorelamoError> {
        let logs_dir = self.shared.root.join("logs");
        if !logs_dir.exists() {
            return Ok(String::new());
        }

        let mut files: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(&logs_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "log") {
                if let Some(ref date_str) = date {
                    if path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|name| name.contains(date_str))
                    {
                        files.push(path);
                    }
                } else {
                    files.push(path);
                }
            }
        }
        files.sort();

        let mut output = String::new();
        for file_path in files {
            let content = std::fs::read_to_string(&file_path)?;
            output.push_str(&content);
            if !content.ends_with('\n') {
                output.push('\n');
            }
        }
        Ok(output)
    }

    pub fn clear_logs_direct(&self) -> Result<(), CorelamoError> {
        let logs_dir = self.shared.root.join("logs");
        if !logs_dir.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(&logs_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "log") {
                std::fs::remove_file(&path)?;
            }
        }
        Ok(())
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
            match self.shared.docs.get(id) {
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

    pub fn resolve_hits_direct(
        &self,
        hits: Vec<SearchHit>,
        return_fields: Option<&IndexMap<String, bool>>,
        policy: &IndexPolicy,
    ) -> Result<Vec<SearchDocumentHit>, CorelamoError> {
        self.ensure_readable()?;
        let mut results = Vec::with_capacity(hits.len());
        for hit in hits {
            let Some(external_id) = self
                .shared
                .internal_to_external
                .get(&hit.doc_id)
                .map(|r| r.value().clone())
            else {
                continue;
            };
            let Some(doc) = self.shared.docs.get(&external_id) else {
                continue;
            };
            results.push(SearchDocumentHit {
                external_id: doc.external_id.clone(),
                internal_id: doc.internal_id,
                score: hit.score,
                fields: visible_fields(&doc.fields, policy, return_fields),
            });
        }
        Ok(results)
    }

    pub(crate) fn send_raw(&self, cmd: ShardCmd) -> Result<(), (CorelamoError, ShardCmd)> {
        self.tx.send(cmd).map_err(|e| (self.dead(), e.0))
    }

    fn dead(&self) -> CorelamoError {
        CorelamoError::Internal(format!("shard {} is not running", self.id))
    }

    async fn call<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> ShardCmd,
    ) -> Result<T, CorelamoError> {
        let (rtx, rrx) = oneshot::channel();
        self.tx.send(make(rtx)).map_err(|_| self.dead())?;
        rrx.await.map_err(|_| self.dead())
    }

    pub async fn insert(
        &self,
        inputs: Vec<DocumentInput>,
        user: String,
    ) -> Result<InsertReport, CorelamoError> {
        self.call(|resp| ShardCmd::Insert { user, inputs, resp })
            .await?
    }
    pub async fn flush(&self) -> Result<(), CorelamoError> {
        self.call(|resp| ShardCmd::Flush { resp }).await?
    }
    pub async fn upsert(
        &self,
        inputs: Vec<DocumentInput>,
        user: String,
    ) -> Result<InsertReport, CorelamoError> {
        self.call(|resp| ShardCmd::Upsert { user, inputs, resp })
            .await?
    }
    pub async fn replace(
        &self,
        inputs: Vec<DocumentInput>,
        user: String,
    ) -> Result<ReplaceReport, CorelamoError> {
        self.call(|resp| ShardCmd::Replace { user, inputs, resp })
            .await?
    }
    pub async fn delete(
        &self,
        ids: Vec<String>,
        user: String,
    ) -> Result<DeleteReport, CorelamoError> {
        self.call(|resp| ShardCmd::Delete { user, ids, resp })
            .await?
    }
    pub async fn set_policy(&self, policy: IndexPolicy, user: String) -> Result<(), CorelamoError> {
        self.call(|resp| ShardCmd::SetPolicy { user, policy, resp })
            .await?
    }
    pub async fn set_config(
        &self,
        options: DatabaseOptions,
        user: String,
    ) -> Result<(), CorelamoError> {
        self.call(|resp| ShardCmd::SetConfig {
            user,
            options,
            resp,
        })
        .await?
    }
    pub async fn start(&self) -> Result<(), CorelamoError> {
        self.call(|resp| ShardCmd::Start { resp }).await?
    }
    pub async fn stop(&self) -> Result<(), CorelamoError> {
        self.call(|resp| ShardCmd::Stop { resp }).await?
    }
    pub async fn clear(&self) -> Result<(), CorelamoError> {
        self.call(|resp| ShardCmd::Clear { resp }).await?
    }
    pub async fn backup_full(
        &self,
        shard_backup_path: PathBuf,
        backup_id: String,
        user: String,
    ) -> Result<BackupManifest, CorelamoError> {
        self.call(|resp| ShardCmd::BackupFull {
            shard_backup_path,
            backup_id,
            resp,
        })
        .await?
    }
    pub async fn list_backups(&self) -> Result<Vec<BackupManifest>, CorelamoError> {
        self.call(|resp| ShardCmd::ListBackups { resp }).await?
    }
    pub async fn backup_incremental(
        &self,

        shard_backup_path: PathBuf,
        backup_id: String,
    ) -> Result<Option<BackupManifest>, CorelamoError> {
        self.call(|resp| ShardCmd::BackupIncremental {
            shard_backup_path,
            backup_id,
            resp,
        })
        .await?
    }

    pub async fn restore_backup(&self, backup_id: &str, user: String) -> Result<(), CorelamoError> {
        let backup_id = backup_id.to_string();
        self.call(move |resp| ShardCmd::Restore {
            user,
            backup_id,
            resp,
        })
        .await?
    }
    //reindex
    pub(crate) fn command_sender(&self) -> Sender<ShardCmd> {
        self.tx.clone()
    }
}

struct AliveGuard(Arc<AtomicBool>);
impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub fn spawn(
    mut shard: ShardDb,
    queue_depth: usize,
    bootable: bool,
) -> Result<(ShardHandle, JoinHandle<()>), CorelamoError> {
    let id = shard.shard_id();
    let progress = shard.progress();
    let alive = Arc::new(AtomicBool::new(true));
    let shared = shard.shared_state();
    let analyzer = Analyzer::new();

    let (tx, rx) = bounded(queue_depth.max(1));
    let (boot_tx, boot_rx) = bounded(1);

    let alive_worker = alive.clone();
    let shared_worker = shared.clone();
    let join = thread::Builder::new()
        .name(format!("shard-{}", id))
        .spawn(move || {
            let _guard = AliveGuard(alive_worker);

            let started = if bootable { shard.start() } else { Ok(()) };

            let ok = started.is_ok();
            shared_worker
                .is_running
                .store(bootable && ok, Ordering::Release);
            let _ = boot_tx.send(started);
            if ok {
                run(shard, rx, shared_worker);
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
            analyzer,
            shared,
        },
        join,
    ))
}

fn run(mut shard: ShardDb, rx: Receiver<ShardCmd>, shared: Arc<SharedShardState>) {
    const MAX_BATCH: usize = 32;
    let mut batch: Vec<ShardCmd> = Vec::with_capacity(MAX_BATCH);

    while let Ok(first) = rx.recv() {
        batch.push(first);
        batch.extend(rx.try_iter().take(MAX_BATCH - 1));
        for cmd in batch.drain(..) {
            match cmd {
                ShardCmd::Insert { inputs, resp, user } => {
                    let _ = resp.send(shard.insert(inputs, user));
                }
                ShardCmd::Flush { resp } => {
                    let _ = resp.send(shard.flush());
                }
                ShardCmd::SetConfig {
                    options,
                    resp,
                    user,
                } => {
                    let _ = resp.send(shard.set_options(options, user));
                }
                ShardCmd::SetPolicy { policy, resp, user } => {
                    let _ = resp.send(shard.set_policy(policy, user));
                }
                ShardCmd::Upsert { inputs, resp, user } => {
                    let _ = resp.send(shard.upsert(inputs, user));
                }
                ShardCmd::Replace { inputs, resp, user } => {
                    let _ = resp.send(shard.replace(inputs, user));
                }
                ShardCmd::Delete { ids, resp, user } => {
                    let _ = resp.send(shard.delete(ids, user));
                }
                ShardCmd::PrepareReindex { resp } => {
                    let _ = resp.send(shard.prepare_reindex());
                }
                ShardCmd::CommitReindex { done, resp } => {
                    let _ = resp.send(shard.commit_reindex(done));
                }
                ShardCmd::Start { resp } => {
                    let result = shard.start();
                    shared.is_running.store(result.is_ok(), Ordering::Release);
                    let _ = resp.send(result);
                }
                ShardCmd::Stop { resp } => {
                    let result = shard.stop();
                    shared.is_running.store(false, Ordering::Release);
                    let _ = resp.send(result);
                }
                ShardCmd::Shutdown { resp } => {
                    let result = shard.stop();
                    shared.is_running.store(false, Ordering::Release);
                    let _ = resp.send(result);
                    return;
                }
                ShardCmd::Clear { resp } => {
                    shared.is_clearing.store(true, Ordering::Release);
                    let result = shard.clear();
                    shared.is_clearing.store(false, Ordering::Release);
                    let _ = resp.send(result);
                }
                ShardCmd::BackupFull {
                    shard_backup_path,
                    backup_id,
                    resp,
                } => {
                    shared.is_backing_up.store(true, Ordering::Release);
                    let result = shard.backup_full(shard_backup_path, backup_id);
                    shared.is_backing_up.store(false, Ordering::Release);
                    if let Ok(manifest) = &result {
                        *shared
                            .last_backup_id
                            .write()
                            .unwrap_or_else(|e| e.into_inner()) = Some(manifest.backup_id.clone());
                        shared
                            .last_backup_at
                            .store(manifest.created_at, Ordering::Release);
                    }
                    let _ = resp.send(result);
                }
                ShardCmd::BackupIncremental {
                    shard_backup_path,
                    backup_id,
                    resp,
                } => {
                    shared.is_backing_up.store(true, Ordering::Release);
                    let result = shard.backup_incremental(shard_backup_path, backup_id);
                    shared.is_backing_up.store(false, Ordering::Release);
                    match &result {
                        Ok(Some(manifest)) => {
                            *shared
                                .last_backup_id
                                .write()
                                .unwrap_or_else(|e| e.into_inner()) =
                                Some(manifest.backup_id.clone());
                            shared
                                .last_backup_at
                                .store(manifest.created_at, Ordering::Release);
                        }
                        Ok(None) => {}
                        Err(_) => {}
                    }
                    let _ = resp.send(result);
                }

                ShardCmd::Restore {
                    user,
                    resp,
                    backup_id,
                } => {
                    shared.is_restoring.store(true, Ordering::Release);
                    let result = shard.restore_backup(&backup_id);
                    shared.is_restoring.store(false, Ordering::Release);
                    let _ = resp.send(result);
                }
                ShardCmd::ListBackups { resp } => {
                    let _ = resp.send(shard.list_backups());
                }
            }
        }
    }
    let _ = shard.stop();
}
