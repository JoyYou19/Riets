//FIX: sis ir tik AI generated pamats visam jutos loti slikti par so bet man vajag pacakareties lai
//saprastu, ja izradisies hujna mainisu - nomrunds
use std::{ io, mem::transmute, sync::{ Arc, Mutex }, thread };

use core_index::document::IndexPolicy;
use tokio::sync::{ mpsc, oneshot, watch };

use core_core::{
    CorelamoDatabase,
    DatabaseOptions,
    DatabaseStats,
    command_reponse_definitions::SearchCommand,
};
use core_protocol::errors::CorelamoError;
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


use crate::db_actor;

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
    StopDatabse {
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
        reply: Reply<Vec<(String, Result<(), CorelamoError>)>>,
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

    pub async fn upsert(
        &self,
        docs: Vec<DocumentInput>
    ) -> Result<Vec<(String, Result<(), CorelamoError>)>, CorelamoError> {
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
        self.call(|reply| DbCommand::StopDatabse { reply }).await
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

    let join = thread::Builder
        ::new()
        .name(format!("db-actor-{name}"))
        .spawn(move || {
            actor_loop(db, &mut rx, &name);
            tracing::info!(db_actor=%name, "exiting");
        })
        .expect("failed to spawn db actor thread");

    (DbHandle { tx }, join)
}

fn actor_loop(mut db: CorelamoDatabase, rx: &mut mpsc::Receiver<DbCommand>, name: &str) {
    let reindexing_rx = db.reindexing_receiver();
    let db = Arc::new(Mutex::new(db));

    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            //core commands
            DbCommand::Search { cmd, reply } => {
                let mut db = db.lock().expect("db actor mutex poisoned");
                let result = if db.is_running() {
                    db.search(&cmd).map_err(|e|{
                        tracing::error!(error=%e, "Search failed");
                        CorelamoError::Internal(format!("search failed: {e}"))}

                    )
                } else {
                    Err({
                         tracing::error!(name=%name, "Database is not running");
                        CorelamoError::DatabaseNotRunning(format!("database {name} is not running"))
                })
                };
                let _ = reply.send(result);
            }
            DbCommand::Insert { docs, reply } => {
                let mut db = db.lock().expect("db actor mutex poisoned");
                let result = if db.is_running() {
                    db.put_documents_parallel(docs).map_err(|e| {
                        tracing::error!(error=%e,"Insert failed");
                        CorelamoError::Internal(format!("insert failed: {e}"))
                    })
                } else {
                    Err({
                        tracing::error!(name=%name, "Database not started");
                        CorelamoError::DatabaseNotRunning(format!("database {name} is not started"))
                    })
                };
                let _ = reply.send(result);
            }

            DbCommand::Retrieve { ids, reply } => {
                let mut db = db.lock().expect("db actor mutex poisoned");
                let result = if db.is_running() {
                    let mut out = Vec::with_capacity(ids.len());
                    let mut failed = None;
                    for id in ids {
                        match db.get_document(&id) {
                            Ok(doc) => out.push((id, doc)),
                            Err(e) => {
                                failed = Some(
                                    CorelamoError::Internal({
                                        tracing::error!(error=%e,"failed to get document");
                                        format!("failed to get document '{id}': {e}")
                                    })
                                );
                                break;
                            }
                        }
                    }
                    match failed {
                        Some(e) => Err(e),
                        None => Ok(out),
                    }
                } else {
                    Err({
                        tracing::error!(name=%name, "Database is not running");
                        CorelamoError::DatabaseNotRunning(format!("database {name} is not running"))
                    })
                };
                let _ = reply.send(result);
            }

            DbCommand::Replace { docs, reply } => {
                let mut db = db.lock().expect("db actor mutex poisoned");
                let result = if db.is_running() {
                    replace_docs(&mut db, docs)
                } else {
                    Err({
                        tracing::error!(name=%name, "Database is not running");
                        CorelamoError::DatabaseNotRunning(format!("database {name} is not started"))
                    })
                };
                let _ = reply.send(result);
            }

            DbCommand::Delete { ids, reply } => {
                let mut db = db.lock().expect("db actor mutex poisoned");
                let result = if db.is_running() {
                    delete_docs(&mut db, ids)
                } else {
                    Err({
                        tracing::error!(name=%name, "Database is not running");
                        CorelamoError::DatabaseNotRunning(format!("database {name} is not started"))
                    })
                };
                let _ = reply.send(result);
            }

            DbCommand::Upsert { docs, reply } => {
                let mut db = db.lock().expect("db actor mutex poisoned");
                let result = if db.is_running() {
                    let mut out = Vec::with_capacity(docs.len());
                    for doc in docs {
                        let id = doc.external_id.clone();
                        let r = db
                            .upsert_document(doc)
                            .map_err(|e| CorelamoError::Internal(e.to_string()));
                        out.push((id, r));
                    }
                    Ok(out)
                } else {
                    Err({
                        tracing::error!(name=%name, "Database is not running");
                        CorelamoError::DatabaseNotRunning(format!("database {name} is not running"))
                    })
                };
                let _ = reply.send(result);
            }

            DbCommand::Reindex { reply } => {
                let is_running = db.lock().expect("db actor mutex poisoned").is_running();
                if !is_running {
                    let _ = reply.send(
                        Err({
                            tracing::error!(name=%name, "Database is not running");
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
                        tracing::error!(error=%e, "Reindex failed");
                    }
                });
                let _ = reply.send(Ok(()));
            }

            //maintnance/config commands
            DbCommand::Shutdown { reply } => {
                let mut db = db.lock().expect("db actor mutex poisoned");
                let result = db
                    .shutdown()
                    .map_err(|e|{
                        tracing::error!(error=%e, "Shutdown failed");
                         CorelamoError::Internal(format!("shutdown failed: {e}"))});
                let _ = reply.send(result);
                return;
            }
            DbCommand::GetPolicy { reply } => {
                let mut db = db.lock().expect("db actor mutex poisoned");
                let _ = reply.send(Ok(db.policy().clone()));
            }
            DbCommand::SetPolicy { policy, reply } => {
                let mut db = db.lock().expect("db actor mutex poisoned");
                let _ = reply.send(
                    db
                        .set_policy(policy)
                        .map_err(|e| {
                            CorelamoError::Internal(format!("set policy failed on '{name}': {e}"))
                        })
                );
            }
            DbCommand::Status { reply } => {
                let result = match db.try_lock() {
                    Ok(mut db) => {
                        if db.is_running() {
                            db.stats()
                                .map(|mut stats| {
                                    stats.reindexing = reindexing_rx.borrow().clone();
                                    stats
                                })
                                .map_err(|e| CorelamoError::Internal(format!("stats failed: {e}")))
                        } else {
                            Err({
                                tracing::error!(name=%name, "Database is not running");
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
            DbCommand::GetOptions { reply } => {
                let mut db = db.lock().expect("db actor mutex poisoned");
                let _ = reply.send(Ok(db.options().clone()));
            }
            DbCommand::SetOptions { options, reply } => {
                let mut db = db.lock().expect("db actor mutex poisoned");
                let result = db.set_options(options).map(|()| db.is_running());
                let _ = reply.send(result);
            }

            DbCommand::StartDatabase { reply } => {
                let mut db = db.lock().expect("db actor mutex poisoned");
                let result = if db.is_running() {
                    Ok(DatabasePowerButtonOutcome::Nochange)
                } else {
                    db.start().map(|()| DatabasePowerButtonOutcome::Changed)
                };
                let _ = reply.send(result);
            }
            DbCommand::StopDatabse { reply } => {
                let mut db = db.lock().expect("db actor mutex poisoned");
                let result = if !db.is_running() {
                    Ok(DatabasePowerButtonOutcome::Nochange)
                } else {
                    db.stop().map(|()| DatabasePowerButtonOutcome::Changed)
                };
                let _ = reply.send(result);
            }

            DbCommand::Restart { reply } => {
                let mut db = db.lock().expect("db actor mutex poisoned");
                let _ = reply.send(db.restart());
            }
            DbCommand::IsRunning { reply } => {
                let mut db = db.lock().expect("db actor mutex poisoned");
                let _ = reply.send(Ok(db.is_running()));
            }
        }
    }

    tracing::warn!(db_actor=%name, "channel closed without shutdown,stopping");
    match db.lock() {
        Ok(mut db) => {
            if let Err(e) = db.stop() {
                tracing::error!(db_actor=%name, error=%e, "stop failed");
            }
        }
        Err(e) => {
            tracing::error!(db_actor=%name,error=%e, "mutex poisoned,could not stop");
        }
    }
}

//helper funcion so that the call looks more readable
fn replace_docs(
    db: &mut CorelamoDatabase,
    docs: Vec<DocumentInput>
) -> Result<ReplaceReport, CorelamoError> {
    let mut replaced = 0;
    let mut not_found = Vec::new();

    for doc in docs {
        let exists = db
            .get_document(&doc.external_id)
            .map_err(|e|{
            tracing::error!(error=%e,"existence check failed");
            CorelamoError::Internal(format!("existence check failed: {e}"))})?
            .is_some();

        if exists {
            db
                .upsert_document(doc)
                .map_err(|e| {
                    tracing::error!(error=%e, "Replace failed");
                    CorelamoError::Internal(format!("replace failed: {e}"))})?;
            replaced += 1;
        } else {
            not_found.push(doc.external_id);
        }
    }

    Ok(ReplaceReport {
        replaced,
        not_found,
    })
}

fn delete_docs(db: &mut CorelamoDatabase, ids: Vec<String>) -> Result<DeleteReport, CorelamoError> {
    let mut deleted = 0;
    let mut not_found = Vec::new();

    for id in ids {
        let exists = db
            .get_document(&id)
            .map_err(|e|{
                tracing::error!(error=%e, id=%id, "Failed lookup");
                 CorelamoError::Internal(format!("failed to lookup '{id}': {e}"))
                })?
            .is_some();

        if exists {
            db
                .delete_document(&id)
                .map_err(|e| {
                    tracing::error!(error=%e, id =%id, "failed to delete");
                    CorelamoError::Internal(format!("failed to delete '{id}': {e}"))
                })?;
            deleted += 1;
        } else {
            not_found.push(id);
        }
    }

    Ok(DeleteReport { deleted, not_found })
}
