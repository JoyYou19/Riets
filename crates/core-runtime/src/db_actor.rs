//FIX: sis ir tik AI generated pamats visam jutos loti slikti par so bet man vajag pacakareties lai
//saprastu, ja izradisies hujna mainisu - nomrunds
use std::{ io, thread };

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
            println!("db actor '{name}' exiting");
        })
        .expect("failed to spawn db actor thread");

    (DbHandle { tx }, join)
}

fn actor_loop(mut db: CorelamoDatabase, rx: &mut mpsc::Receiver<DbCommand>, name: &str) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            //core commands
            DbCommand::Search { cmd, reply } => {
                let result = if db.is_running() {
                    db.search(&cmd).map_err(|e|
                        CorelamoError::Internal(format!("search failed: {e}"))
                    )
                } else {
                    Err(
                        CorelamoError::DatabaseNotRunning(format!("database {name} is not running"))
                    )
                };
                let _ = reply.send(result);
            }
            DbCommand::Insert { docs, reply } => {
                let result = if db.is_running() {
                    db.put_documents_parallel(docs).map_err(|e|
                        CorelamoError::Internal(format!("insert failed: {e}"))
                    )
                } else {
                    Err(
                        CorelamoError::DatabaseNotRunning(format!("database {name} is not started"))
                    )
                };
                let _ = reply.send(result);
            }

            DbCommand::Retrieve { ids, reply } => {
                let result = if db.is_running() {
                    let mut out = Vec::with_capacity(ids.len());
                    let mut failed = None;
                    for id in ids {
                        match db.get_document(&id) {
                            Ok(doc) => out.push((id, doc)),
                            Err(e) => {
                                failed = Some(
                                    CorelamoError::Internal(
                                        format!("failed to get document '{id}': {e}")
                                    )
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
                    Err(
                        CorelamoError::DatabaseNotRunning(format!("database {name} is not running"))
                    )
                };
                let _ = reply.send(result);
            }

            DbCommand::Replace { docs, reply } => {
                let result = if db.is_running() {
                    replace_docs(&mut db, docs)
                } else {
                    Err(
                        CorelamoError::DatabaseNotRunning(format!("database {name} is not started"))
                    )
                };
                let _ = reply.send(result);
            }

            DbCommand::Delete { ids, reply } => {
                let result = if db.is_running() {
                    delete_docs(&mut db, ids)
                } else {
                    Err(
                        CorelamoError::DatabaseNotRunning(format!("database {name} is not started"))
                    )
                };
                let _ = reply.send(result);
            }

            DbCommand::Upsert { docs, reply } => {
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
                    Err(
                        CorelamoError::DatabaseNotRunning(format!("database {name} is not running"))
                    )
                };
                let _ = reply.send(result);
            }

            DbCommand::Reindex { reply } => {
                //WARN: nukes the index dir and rebuilds from every stored document.
                //minutes on a big db, and everything queues behind it.
                let result = if db.is_running() {
                    db.reindex().map_err(|e|
                        CorelamoError::Internal(format!("reindex failed: {e}"))
                    )
                } else {
                    Err(
                        CorelamoError::DatabaseNotRunning(format!("database {name} is not running"))
                    )
                };
                let _ = reply.send(result);
            }

            //maintnance/config commands
            DbCommand::Shutdown { reply } => {
                let result = db
                    .shutdown()
                    .map_err(|e| CorelamoError::Internal(format!("shutdown failed: {e}")));
                let _ = reply.send(result);
                return;
            }
            DbCommand::GetPolicy { reply } => {
                let _ = reply.send(Ok(db.policy().clone()));
            }
            DbCommand::SetPolicy { policy, reply } => {
                let _ = reply.send(
                    db
                        .set_policy(policy)
                        .map_err(|e| {
                            CorelamoError::Internal(format!("set policy failed on '{name}': {e}"))
                        })
                );
            }
            DbCommand::Status { reply } => {
                let result = db
                    .stats()
                    .map_err(|e| CorelamoError::Internal(format!("stats failed: {e}")));
                let _ = reply.send(result);
            }
            DbCommand::GetOptions { reply } => {
                let _ = reply.send(Ok(db.options().clone()));
            }
            DbCommand::SetOptions { options, reply } => {
                let result = db.set_options(options).map(|()| db.is_running());
                let _ = reply.send(result);
            }

            DbCommand::StartDatabase { reply } => {
                let result = if db.is_running() {
                    Ok(DatabasePowerButtonOutcome::Nochange)
                } else {
                    db.start().map(|()| DatabasePowerButtonOutcome::Changed)
                };
                let _ = reply.send(result);
            }
            DbCommand::StopDatabse { reply } => {
                let result = if !db.is_running() {
                    Ok(DatabasePowerButtonOutcome::Nochange)
                } else {
                    db.stop().map(|()| DatabasePowerButtonOutcome::Changed)
                };
                let _ = reply.send(result);
            }

            DbCommand::Restart { reply } => {
                let _ = reply.send(db.restart());
            }
            DbCommand::IsRunning { reply } => {
                let _ = reply.send(Ok(db.is_running()));
            }
        }
    }

    eprintln!("db actor '{name}': channel closed without shutdown, stopping");
    if let Err(e) = db.stop() {
        eprintln!("db actor '{name}': stop failed: {e}");
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
            .map_err(|e| CorelamoError::Internal(format!("existence check failed: {e}")))?
            .is_some();

        if exists {
            db
                .upsert_document(doc)
                .map_err(|e| CorelamoError::Internal(format!("replace failed: {e}")))?;
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
            .map_err(|e| CorelamoError::Internal(format!("failed to lookup '{id}': {e}")))?
            .is_some();

        if exists {
            db
                .delete_document(&id)
                .map_err(|e| CorelamoError::Internal(format!("failed to delete '{id}': {e}")))?;
            deleted += 1;
        } else {
            not_found.push(id);
        }
    }

    Ok(DeleteReport { deleted, not_found })
}
