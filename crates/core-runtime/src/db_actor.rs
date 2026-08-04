use core_core::{
    CorelamoDatabase,
    DatabaseOptions,
    DatabaseStats,
    command_reponse_definitions::{ LookupCommand, SearchCommand },
};
use core_index::{ document::IndexPolicy, lsm::index_worker::ReindexGuard };
use core_protocol::errors::{ CorelamoError, DocFailure, FailReason };
use core_storage::{
    document_store::StoredDocument,
    search_database::{
        DatabasePowerButtonOutcome,
        DeleteReport,
        DocumentInput,
        InsertReport,
        ReplaceReport,
        SearchDocumentHit,
    },
};
use slog::{ error, info, warn };
use slog_scope::logger;
use std::{ collections::BTreeMap, sync::{ Arc, Mutex }, thread };
use tokio::sync::{ mpsc, oneshot };

//oneshot chanel - creates a chanel for a single message (command)
type Reply<T> = oneshot::Sender<Result<T, CorelamoError>>;

//Commands
pub enum DbCommand {
    Search {
        cmd: SearchCommand,
        reply: Reply<Vec<SearchDocumentHit>>,
    },
    Lookup {
        cmd: LookupCommand,
        //                 id             fields                not found
        reply: Reply<(Vec<(String, BTreeMap<String, String>)>, Vec<String>)>,
    },
    GetPolicy {
        reply: Reply<IndexPolicy>,
    },
    Shutdown {
        reply: Reply<()>,
    },
    Clear {
        reply: Reply<()>,
    },
    GetLogs {
        reply: Reply<String>,
    },
    ClearLogs {
        reply: Reply<()>,
    },
    Retrieve {
        ids: Vec<String>,
        reply: Reply<Vec<(String, Option<StoredDocument>)>>,
    },
    Status {
        reply: Reply<DatabaseStats>,
    },
    GetOptions {
        reply: Reply<DatabaseOptions>,
    },
    StartDatabase {
        reply: Reply<DatabasePowerButtonOutcome>,
    },
    StopDatabase {
        reply: Reply<DatabasePowerButtonOutcome>,
    },
    Restart {
        reply: Reply<()>,
    },
    SetPolicy {
        policy: IndexPolicy,
        reply: Reply<()>,
    },
    IsRunning {
        reply: Reply<bool>,
    },
    Upsert {
        docs: Vec<DocumentInput>,
        reply: Reply<Vec<(usize, String, Result<(), CorelamoError>)>>,
    },
    Replace {
        docs: Vec<DocumentInput>,
        reply: Reply<ReplaceReport>,
    },
    Insert {
        docs: Vec<DocumentInput>,
        reply: Reply<InsertReport>,
    },
    Delete {
        ids: Vec<String>,
        reply: Reply<DeleteReport>,
    },
    Reindex {
        reply: Reply<()>,
    },
    SetOptions {
        options: DatabaseOptions,
        //this bool is for detecting wether to tell the user to restart the database or not
        reply: Reply<bool>,
    },
}

//Each database has a handler
#[derive(Clone)]
pub struct DbHandle {
    tx: mpsc::Sender<DbCommand>,
}

impl DbHandle {
    //for now im considering this to be dark magic
    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> DbCommand) -> Result<T, CorelamoError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(make(reply_tx)).await
            .map_err(|_| CorelamoError::Internal("database actor is gone".into()))?;
        reply_rx.await.map_err(|_|
            CorelamoError::Internal("database actor dropped the reply".into())
        )?
    }

    pub async fn search(
        &self,
        cmd: SearchCommand
    ) -> Result<Vec<SearchDocumentHit>, CorelamoError> {
        self.call(|reply| DbCommand::Search { cmd, reply }).await
    }

    pub async fn insert(&self, docs: Vec<DocumentInput>) -> Result<InsertReport, CorelamoError> {
        self.call(|reply| DbCommand::Insert { docs, reply }).await
    }

    pub async fn delete(&self, ids: Vec<String>) -> Result<DeleteReport, CorelamoError> {
        self.call(|reply| DbCommand::Delete { ids, reply }).await
    }

    pub async fn reindex(&self) -> Result<(), CorelamoError> {
        self.call(|reply| DbCommand::Reindex { reply }).await
    }

    pub async fn clear(&self) -> Result<(), CorelamoError> {
        self.call(|reply| DbCommand::Clear { reply }).await
    }

    pub async fn get_logs(&self) -> Result<String, CorelamoError> {
        self.call(|reply| DbCommand::GetLogs { reply }).await
    }

    pub async fn clear_logs(&self) -> Result<(), CorelamoError> {
        self.call(|reply| DbCommand::ClearLogs { reply }).await
    }

    pub async fn upsert(
        &self,
        docs: Vec<DocumentInput>
    ) -> Result<Vec<(usize, String, Result<(), CorelamoError>)>, CorelamoError> {
        self.call(|reply| DbCommand::Upsert { docs, reply }).await
    }

    pub async fn replace(&self, docs: Vec<DocumentInput>) -> Result<ReplaceReport, CorelamoError> {
        self.call(|reply| DbCommand::Replace { docs, reply }).await
    }

    pub async fn get_policy(&self) -> Result<IndexPolicy, CorelamoError> {
        self.call(|reply| DbCommand::GetPolicy { reply }).await
    }
    pub async fn set_policy(&self, policy: IndexPolicy) -> Result<(), CorelamoError> {
        self.call(|reply| DbCommand::SetPolicy { policy, reply }).await
    }

    pub async fn shutdown(&self) -> Result<(), CorelamoError> {
        self.call(|reply| DbCommand::Shutdown { reply }).await
    }

    pub async fn lookup(
        &self,
        cmd: LookupCommand
    ) -> Result<(Vec<(String, BTreeMap<String, String>)>, Vec<String>), CorelamoError> {
        self.call(|reply| DbCommand::Lookup { cmd, reply }).await
    }

    pub async fn stats(&self) -> Result<DatabaseStats, CorelamoError> {
        self.call(|reply| DbCommand::Status { reply }).await
    }

    pub async fn options(&self) -> Result<DatabaseOptions, CorelamoError> {
        self.call(|reply| DbCommand::GetOptions { reply }).await
    }

    pub async fn retrieve(
        &self,
        ids: Vec<String>
    ) -> Result<Vec<(String, Option<StoredDocument>)>, CorelamoError> {
        self.call(|reply| DbCommand::Retrieve { ids, reply }).await
    }

    pub async fn start(&self) -> Result<DatabasePowerButtonOutcome, CorelamoError> {
        self.call(|reply| DbCommand::StartDatabase { reply }).await
    }

    pub async fn stop(&self) -> Result<DatabasePowerButtonOutcome, CorelamoError> {
        self.call(|reply| DbCommand::StopDatabase { reply }).await
    }

    pub async fn restart(&self) -> Result<(), CorelamoError> {
        self.call(|reply| DbCommand::Restart { reply }).await
    }

    pub async fn is_running(&self) -> Result<bool, CorelamoError> {
        self.call(|reply| DbCommand::IsRunning { reply }).await
    }
    pub async fn set_options(&self, options: DatabaseOptions) -> Result<bool, CorelamoError> {
        self.call(|reply| DbCommand::SetOptions { options, reply }).await
    }
}

pub fn spawn_db_actor(db: CorelamoDatabase, name: String) -> (DbHandle, thread::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<DbCommand>(64);
    let log = slog_scope::logger();
    let join = thread::Builder
        ::new()
        .name(format!("db-actor-{name}"))
        .spawn(move || {
            actor_loop(db, &mut rx, &name);
            info!(log,"exiting";"db_actor"=>%name);
        })
        .expect("failed to spawn db actor thread");

    (DbHandle { tx }, join)
}

fn actor_loop(db: CorelamoDatabase, rx: &mut mpsc::Receiver<DbCommand>, name: &str) {
    let progress = db.progress();
    let db = Arc::new(Mutex::new(db));
    let log = slog_scope::logger();
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            DbCommand::Search { cmd, reply } =>
                with_running(&db, name, reply, |db| {
                    db.search(&cmd).map_err(|e|
                        CorelamoError::Internal(format!("search failed: {e}"))
                    )
                }),

            DbCommand::Lookup { cmd, reply } => {
                with_running(&db, name, reply, |db| db.lookup(&cmd));
            }

            DbCommand::Insert { docs, reply } =>
                with_running(&db, name, reply, |db| {
                    db.put_documents_parallel(docs).map_err(|e|
                        CorelamoError::Internal(format!("insert failed: {e}"))
                    )
                }),

            DbCommand::Retrieve { ids, reply } =>
                with_running(&db, name, reply, |db| {
                    let mut out = Vec::with_capacity(ids.len());
                    for id in ids {
                        let doc = db
                            .get_document(&id)
                            .map_err(|e| {
                                CorelamoError::Internal(
                                    format!("failed to get document '{id}': {e}")
                                )
                            })?;
                        out.push((id, doc));
                    }
                    Ok(out)
                }),

            DbCommand::Replace { docs, reply } => {
                with_running(&db, name, reply, |db| replace_docs(db, docs));
            }

            DbCommand::Delete { ids, reply } => {
                with_running(&db, name, reply, |db| delete_docs(db, ids));
            }

            DbCommand::Upsert { docs, reply } =>
                with_running(&db, name, reply, |db| {
                    let mut out = Vec::with_capacity(docs.len());
                    for (index, doc) in docs.into_iter().enumerate() {
                        let id = doc.external_id.clone();
                        let r = db
                            .upsert_document(doc)
                            .map_err(|e| CorelamoError::Internal(e.to_string()));
                        out.push((index, id, r));
                    }
                    Ok(out)
                }),

            DbCommand::Reindex { reply } => {
                if progress.phase().is_running() {
                    info!(log, "cancelling in-flight reindex");
                        let _ = reply.send(
                            Err(
                                CorelamoError::Busy(
                                    format!(
                                        "database '{name}': previous reindex did not stop in time"
                                    )
                                )
                            )
                        );
                        continue;
                    
                }
                let params = {
                    let mut guard = db.lock().expect("db actor mutex poisoned");
                    if !guard.is_running() {
                        let _ = reply.send(
                            Err(
                                CorelamoError::DatabaseNotRunning(
                                    format!("database {name} is not running")
                                )
                            )
                        );
                        continue;
                    }
                    let total = guard.document_count_hint();
                    // try_begin is the overlap guard: check and claim in one atomic step.
                    if !progress.try_begin(total) {
                        let _ = reply.send(
                            Err(
                                CorelamoError::Busy(
                                    format!("database '{name}' is already reindexing")
                                )
                            )
                        );
                        continue;
                    }
                    // Stop compaction now: merges on the old index are discarded at swap
                    // and compete with staging for I/O.
                    if let Err(e) = guard.pause_compaction() {
                        warn!(log, "could not pause compaction"; "error" => %e);
                    }
                    guard.reindex_params()
                }; // lock released

                let db_handle = db.clone();
                let thread_log = log.clone();
                let guard = ReindexGuard::new(progress.clone()); // Failed on panic or early return

                std::thread::spawn(move || {
                    match CorelamoDatabase::build_staging_index(&params) {
                        Ok(watermark) => {
                            let mut db = db_handle.lock().expect("db actor mutex poisoned");
                            match db.reindex_swap(watermark) {
                                // reindex_swap sets Complete/Failed itself; settle the
                                // guard so Drop does not overwrite it.
                                Ok(()) => guard.succeed(),
                                Err(_) => guard.fail(),
                            }
                        }
                        Err(e) => {
                            error!(thread_log, "reindex staging failed"; "error" => %e);
                            let mut db = db_handle.lock().expect("db actor mutex poisoned");
                            // staging failed with the database still intact; just restart
                            // the compaction we paused.
                            if let Err(e) = db.start() {
                                error!(thread_log, "resume failed"; "error" => %e);
                            }
                            guard.fail();
                        }
                    }
                });

                let _ = reply.send(Ok(()));
            }

            DbCommand::Clear { reply } => {
                let result = {
                    let mut db = db.lock().expect("db actor mutex poisoned");
                    db.clear().map_err(|e| {
                        error!(logger(), "Database Clear failed"; "error" => %e);
                        CorelamoError::Internal(format!("failed to clear database {name}: {e}"))
                    })
                };
                let _ = reply.send(result);
            }

            DbCommand::GetLogs { reply } => {
                let result = {
                    let db = db.lock().expect("db actor mutex poisoned");
                    db.get_logs()
                };
                let _ = reply.send(result);
            }

            DbCommand::ClearLogs { reply } => {
                let result = {
                    let mut db = db.lock().expect("db actor mutex poisoned");
                    db.clear_logs().map_err(|e| {
                        error!(logger(), "Database Clear failed"; "error" => %e);
                        CorelamoError::Internal(format!("failed to clear database {name}: {e}"))
                    })
                };
                let _ = reply.send(result);
            }

            DbCommand::Shutdown { reply } => {
                let mut db = db.lock().expect("db actor mutex poisoned");
                let result = db.shutdown().map_err(|e| {
                    error!(logger(), "Shutdown failed";"error"=>%e);
                    CorelamoError::Internal(format!("shutdown failed: {e}"))
                });
                let _ = reply.send(result);
                return;
            }

            DbCommand::GetPolicy { reply } => with_db(&db, reply, |db| Ok(db.policy().clone())),

            DbCommand::SetPolicy { policy, reply } =>
                with_db(&db, reply, |db| {
                    db.set_policy(policy).map_err(|e| {
                        CorelamoError::Internal(format!("set policy failed on '{name}': {e}"))
                    })
                }),

            DbCommand::Status { reply } => {
                let snapshot = progress.snapshot();
                let result = match db.try_lock() {
                    Ok(guard) if guard.is_running() =>
                        guard
                            .stats()
                            .map_err(|e| CorelamoError::Internal(format!("stats failed: {e}"))),
                    Ok(_) =>
                        Err(
                            CorelamoError::DatabaseNotRunning(
                                format!("database {name} is not running")
                            )
                        ),
                    // The reindex holds the lock. Report real progress rather than waiting.
                    Err(_) => Ok(CorelamoDatabase::stats_swapping(snapshot, Default::default())),
                };
                let _ = reply.send(result);
            }

            DbCommand::GetOptions { reply } => with_db(&db, reply, |db| Ok(db.options().clone())),

            DbCommand::SetOptions { options, reply } =>
                with_db(&db, reply, |db| {
                    db.set_options(options)?;
                    Ok(db.is_running())
                }),

            DbCommand::StartDatabase { reply } =>
                with_db(&db, reply, |db| {
                    if db.is_running() {
                        Ok(DatabasePowerButtonOutcome::Nochange)
                    } else {
                        db.start().map(|()| DatabasePowerButtonOutcome::Changed)
                    }
                }),

            DbCommand::StopDatabase { reply } =>
                with_db(&db, reply, |db| {
                    if !db.is_running() {
                        Ok(DatabasePowerButtonOutcome::Nochange)
                    } else {
                        db.stop().map(|()| DatabasePowerButtonOutcome::Changed)
                    }
                }),

            DbCommand::Restart { reply } => with_db(&db, reply, |db| db.restart()),

            DbCommand::IsRunning { reply } => with_db(&db, reply, |db| Ok(db.is_running())),
        }
    }

    warn!(log, "channel closed without shutdown,stopping";"db_actor"=>%name);
    match db.lock() {
        Ok(mut db) => {
            if let Err(e) = db.stop() {
                error!(log, "stop failed";"db_actor"=>%name, "error"=>%e);
            }
        }
        Err(e) => {
            error!(log, "mutex poisoned,could not stop";"db_actor"=>%name,"error"=>%e);
        }
    }
}

//checks if the database exists and if it is running
//MARCO:
fn with_running<T>(
    db: &Arc<Mutex<CorelamoDatabase>>,
    name: &str,
    reply: Reply<T>,
    f: impl FnOnce(&mut CorelamoDatabase) -> Result<T, CorelamoError>
) {
    let mut db = db.lock().expect("db actor mutex poisoned");
    let result = if db.is_running() {
        f(&mut db)
    } else {
        Err(CorelamoError::DatabaseNotRunning(format!("database {name} is not running")))
    };
    let _ = reply.send(result);
}

//just checks database
fn with_db<T>(
    db: &Arc<Mutex<CorelamoDatabase>>,
    reply: Reply<T>,
    f: impl FnOnce(&mut CorelamoDatabase) -> Result<T, CorelamoError>
) {
    let mut db = db.lock().expect("db actor mutex poisoned");
    let _ = reply.send(f(&mut db));
}

//helper funcion so that the call looks more readable
fn replace_docs(
    db: &mut CorelamoDatabase,
    docs: Vec<DocumentInput>
) -> Result<ReplaceReport, CorelamoError> {
    let mut replaced = 0;
    let mut failures = Vec::new();
    let log = slog_scope::logger();
    for (index, doc) in docs.into_iter().enumerate() {
        let exists = db
            .get_document(&doc.external_id)
            .map_err(|e| {
                error!(log,"existence check failed";"error"=>%e);
                CorelamoError::Internal(format!("existence check failed: {e}"))
            })?
            .is_some();

        if exists {
            db.upsert_document(doc).map_err(|e| {
                error!(log, "Replace failed";"error"=>%e);
                CorelamoError::Internal(format!("replace failed: {e}"))
            })?;
            replaced += 1;
        } else {
            failures.push(DocFailure::with_id(index, doc.external_id, FailReason::NotFound));
        }
    }

    Ok(ReplaceReport { replaced, failures })
}

fn delete_docs(db: &mut CorelamoDatabase, ids: Vec<String>) -> Result<DeleteReport, CorelamoError> {
    let mut deleted = 0;
    let mut failures = Vec::new();
    let log = slog_scope::logger();
    for (index, id) in ids.into_iter().enumerate() {
        let exists = db
            .get_document(&id)
            .map_err(|e| {
                error!(log, "Failed lookup";"error"=>%e, "id"=>%id);
                CorelamoError::Internal(format!("failed to lookup '{id}': {e}"))
            })?
            .is_some();

        if exists {
            db.delete_document(&id).map_err(|e| {
                error!(log, "failed to delete";"error"=>%e, "id" =>%id);
                CorelamoError::Internal(format!("failed to delete '{id}': {e}"))
            })?;
            deleted += 1;
        } else {
            failures.push(DocFailure::with_id(index, id, FailReason::NotFound));
        }
    }

    Ok(DeleteReport { deleted, failures })
}
