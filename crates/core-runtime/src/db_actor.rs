//FIX: sis ir tik AI generated pamats visam jutos loti slikti par so bet man vajag pacakareties lai
//saprastu, ja izradisies hujna mainisu - nomrunds
use std::{ sync::{ Arc, Mutex }, thread };

use core_index::document::IndexPolicy;
use slog_scope::logger;
use tokio::sync::{mpsc, oneshot};
use slog::{error, info, warn};
use core_core::{
    CorelamoDatabase, DatabaseOptions, DatabaseStats, command_reponse_definitions::SearchCommand,
};
use core_protocol::errors::{CorelamoError, DocFailure, FailReason};
use core_storage::{
    document_store::StoredDocument,
    search_database::{
        DatabasePowerButtonOutcome, DeleteReport, DocumentInput, InsertReport, ReplaceReport,
        SearchDocumentHit,
    },
};



//oneshot chanel - creates a chanel for a single message (command)
type Reply<T> = oneshot::Sender<Result<T, CorelamoError>>;

//Commands
pub enum DbCommand {
    Search {
        cmd: SearchCommand,
        reply: Reply<Vec<SearchDocumentHit>>,
    },
    GetPolicy {
        reply: Reply<IndexPolicy>,
    },
    Shutdown {
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
            .send(make(reply_tx))
            .await
            .map_err(|_| CorelamoError::Internal("database actor is gone".into()))?;
        reply_rx
            .await
            .map_err(|_| CorelamoError::Internal("database actor dropped the reply".into()))?
    }

    pub async fn search(
        &self,
        cmd: SearchCommand,
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

    pub async fn upsert(
        &self,
        docs: Vec<DocumentInput>,
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
        self.call(|reply| DbCommand::SetPolicy { policy, reply })
            .await
    }

    pub async fn shutdown(&self) -> Result<(), CorelamoError> {
        self.call(|reply| DbCommand::Shutdown { reply }).await
    }

    pub async fn stats(&self) -> Result<DatabaseStats, CorelamoError> {
        self.call(|reply| DbCommand::Status { reply }).await
    }

    pub async fn options(&self) -> Result<DatabaseOptions, CorelamoError> {
        self.call(|reply| DbCommand::GetOptions { reply }).await
    }

    pub async fn retrieve(
        &self,
        ids: Vec<String>,
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
        self.call(|reply| DbCommand::SetOptions { options, reply })
            .await
    }
}

pub fn spawn_db_actor(db: CorelamoDatabase, name: String) -> (DbHandle, thread::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<DbCommand>(64);
    let log =slog_scope::logger();
    let join = thread::Builder::new()
        .name(format!("db-actor-{name}"))
        .spawn(move || {
            actor_loop(db, &mut rx, &name);
            info!(log,"exiting";"db_actor"=>%name);
        })
        .expect("failed to spawn db actor thread");

    (DbHandle { tx }, join)
}

fn actor_loop(db: CorelamoDatabase, rx: &mut mpsc::Receiver<DbCommand>, name: &str) {
    let reindexing_rx = db.reindexing_receiver();
    let db = Arc::new(Mutex::new(db));
    let log =slog_scope::logger();
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            DbCommand::Search { cmd, reply } => with_running(&db, name, reply, |db| {
                db.search(&cmd)
                    .map_err(|e| CorelamoError::Internal(format!("search failed: {e}")))
            }),

            DbCommand::Insert { docs, reply } => with_running(&db, name, reply, |db| {
                db.put_documents_parallel(docs)
                    .map_err(|e| CorelamoError::Internal(format!("insert failed: {e}")))
            }),

            DbCommand::Retrieve { ids, reply } => with_running(&db, name, reply, |db| {
                let mut out = Vec::with_capacity(ids.len());
                for id in ids {
                    let doc = db.get_document(&id).map_err(|e| {
                        CorelamoError::Internal(format!("failed to get document '{id}': {e}"))
                    })?;
                    out.push((id, doc));
                }
                Ok(out)
            }),

            DbCommand::Replace { docs, reply } => {
                with_running(&db, name, reply, |db| replace_docs(db, docs))
            }

            DbCommand::Delete { ids, reply } => {
                with_running(&db, name, reply, |db| delete_docs(db, ids))
            }

            DbCommand::Upsert { docs, reply } => with_running(&db, name, reply, |db| {
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
                let is_running = db.lock().expect("db actor mutex poisoned").is_running();
                let log=slog_scope::logger();
                if !is_running {
                    let _ = reply.send(
                        Err({
                            error!(log, "Database is not running";"name"=>%name);
                            CorelamoError::DatabaseNotRunning(
                                format!("database {name} is not running")
                            )
                        })
                    );
                    continue;
                }
                let db_handle = db.clone();
                std::thread::spawn(move || {
                    let mut db = db_handle.lock().expect("db actor mutex poisoned");
                    if let Err(e) = db.reindex() {
                        error!(log, "Reindex failed";"error"=>%e);
                    }
                });
                let _ = reply.send(Ok(()));
            }

            DbCommand::Shutdown { reply } => {
                let mut db = db.lock().expect("db actor mutex poisoned");
                let result = db
                    .shutdown()
                    .map_err(|e|{
                        error!(logger(), "Shutdown failed";"error"=>%e);
                         CorelamoError::Internal(format!("shutdown failed: {e}"))});
                let _ = reply.send(result);
                return;
            }

            DbCommand::GetPolicy { reply } => with_db(&db, reply, |db| Ok(db.policy().clone())),

            DbCommand::SetPolicy { policy, reply } => with_db(&db, reply, |db| {
                db.set_policy(policy).map_err(|e| {
                    CorelamoError::Internal(format!("set policy failed on '{name}': {e}"))
                })
            }),

            DbCommand::Status { reply } => {
                let result = match db.try_lock() {
                    Ok(db) => {
                        if db.is_running() {
                            db.stats()
                                .map(|mut stats| {
                                    stats.reindexing = reindexing_rx.borrow().clone();
                                    stats
                                })
                                .map_err(|e| CorelamoError::Internal(format!("stats failed: {e}")))
                        } else {
                            Err({
                                error!(log, "Database is not running";"name"=>%name);
                                CorelamoError::DatabaseNotRunning(
                                    format!("database {name} is not running")
                                )
                        })
                        }
                    }
                    Err(_) => {
                        // reindex is holding the lock right now — return progress only,
                        // don't wait for it
                        Ok(DatabaseStats {
                            document_count: 0,
                            segment_count: 0,
                            background_compaction_enabled: false,
                            metrics: Default::default(),
                            indexing: Default::default(),
                            reindexing: reindexing_rx.borrow().clone(),
                        })
                    }
                };
                let _ = reply.send(result);
            }

            DbCommand::GetOptions { reply } => with_db(&db, reply, |db| Ok(db.options().clone())),

            DbCommand::SetOptions { options, reply } => with_db(&db, reply, |db| {
                db.set_options(options)?;
                Ok(db.is_running())
            }),

            DbCommand::StartDatabase { reply } => with_db(&db, reply, |db| {
                if db.is_running() {
                    Ok(DatabasePowerButtonOutcome::Nochange)
                } else {
                    db.start().map(|()| DatabasePowerButtonOutcome::Changed)
                }
            }),

            DbCommand::StopDatabase { reply } => with_db(&db, reply, |db| {
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
    f: impl FnOnce(&mut CorelamoDatabase) -> Result<T, CorelamoError>,
) {
    let mut db = db.lock().expect("db actor mutex poisoned");
    let result = if db.is_running() {
        f(&mut db)
    } else {
        Err(CorelamoError::DatabaseNotRunning(format!(
            "database {name} is not running"
        )))
    };
    let _ = reply.send(result);
}

//just checks database
fn with_db<T>(
    db: &Arc<Mutex<CorelamoDatabase>>,
    reply: Reply<T>,
    f: impl FnOnce(&mut CorelamoDatabase) -> Result<T, CorelamoError>,
) {
    let mut db = db.lock().expect("db actor mutex poisoned");
    let _ = reply.send(f(&mut db));
}

//helper funcion so that the call looks more readable
fn replace_docs(
    db: &mut CorelamoDatabase,
    docs: Vec<DocumentInput>,
) -> Result<ReplaceReport, CorelamoError> {
    let mut replaced = 0;
    let mut failures = Vec::new();
    let log=slog_scope::logger();
    for (index, doc) in docs.into_iter().enumerate() {
        let exists = db
            .get_document(&doc.external_id)
            .map_err(|e|{
            error!(log,"existence check failed";"error"=>%e);
            CorelamoError::Internal(format!("existence check failed: {e}"))})?
            .is_some();

        if exists {
            db
                .upsert_document(doc)
                .map_err(|e| {
                    error!(log, "Replace failed";"error"=>%e);
                    CorelamoError::Internal(format!("replace failed: {e}"))})?;
            replaced += 1;
        } else {
            failures.push(DocFailure::with_id(
                index,
                doc.external_id,
                FailReason::NotFound,
            ));
        }
    }

    Ok(ReplaceReport { replaced, failures })
}

fn delete_docs(db: &mut CorelamoDatabase, ids: Vec<String>) -> Result<DeleteReport, CorelamoError> {
    let mut deleted = 0;
    let mut failures = Vec::new();
    let log=slog_scope::logger();
    for (index, id) in ids.into_iter().enumerate() {
        let exists = db
            .get_document(&id)
            .map_err(|e|{
                error!(log, "Failed lookup";"error"=>%e, "id"=>%id);
                 CorelamoError::Internal(format!("failed to lookup '{id}': {e}"))
                })?
            .is_some();

        if exists {
            db
                .delete_document(&id)
                .map_err(|e| {
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
