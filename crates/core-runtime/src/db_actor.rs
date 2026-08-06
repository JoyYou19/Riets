use core_core::{
    DatabaseOptions,
    command_reponse_definitions::SearchCommand,
    shard_manager::ShardManager,
};
use core_index::document::IndexPolicy;
use core_protocol::errors::CorelamoError;
use core_storage::search_database::{DocumentInput, InsertReport, SearchDocumentHit};
use std::sync::Arc;

// ===== not yet ported to sharding =====
// use core_core::command_reponse_definitions::LookupCommand;
// use core_storage::document_store::StoredDocument;
// use core_storage::search_database::{DatabasePowerButtonOutcome, DeleteReport, ReplaceReport};
// use std::collections::BTreeMap;

/// One database. Wraps ShardManager so handlers keep an async API and
/// blocking shard calls stay off the tokio worker threads.
#[derive(Clone)]
pub struct DbHandle {
    manager: Arc<ShardManager>,
    name: String,
}

impl DbHandle {
    pub fn new(manager: ShardManager, name: String) -> Self {
        Self { manager: Arc::new(manager), name }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Runs a blocking ShardManager call on the blocking pool.
    async fn blocking<T, F>(&self, f: F) -> Result<T, CorelamoError>
    where
        F: FnOnce(&ShardManager) -> Result<T, CorelamoError> + Send + 'static,
        T: Send + 'static,
    {
        let m = Arc::clone(&self.manager);
        tokio::task::spawn_blocking(move || f(&m))
            .await
            .map_err(|e| CorelamoError::Internal(format!("db task panicked: {e}")))?
    }

    // ===== working =====

    pub async fn search(
        &self,
        cmd: SearchCommand,
    ) -> Result<Vec<SearchDocumentHit>, CorelamoError> {
        self.blocking(move |m| m.search(&cmd)).await
    }

    pub async fn insert(&self, docs: Vec<DocumentInput>) -> Result<InsertReport, CorelamoError> {
        self.blocking(move |m| m.insert(docs)).await
    }

    pub async fn set_policy(&self, policy: IndexPolicy) -> Result<(), CorelamoError> {
        self.blocking(move |m| m.set_policy_all(policy)).await
    }

    pub async fn set_options(&self, options: DatabaseOptions) -> Result<bool, CorelamoError> {
        self.blocking(move |m| m.set_options_all(options).map(|_| true)).await
    }

    pub async fn shutdown(&self) -> Result<(), CorelamoError> {
        self.blocking(|m| m.shutdown()).await
    }

    // no channel round-trip needed, these just read a field
    pub async fn get_policy(&self) -> Result<IndexPolicy, CorelamoError> {
        Ok(self.manager.policy())
    }

    pub async fn options(&self) -> Result<DatabaseOptions, CorelamoError> {
        Ok(self.manager.options())
    }

    pub async fn is_running(&self) -> Result<bool, CorelamoError> {
        Ok(self.manager.all_alive())
    }

    // ===== needed by handlers, no ShardManager implementation yet =====
    // Each of these needs a matching method on ShardManager first.
    // They return an error instead of hanging so the endpoint fails loudly.

    fn unimplemented<T>(what: &str) -> Result<T, CorelamoError> {
        Err(CorelamoError::Internal(format!(
            "{what} is not implemented for sharded databases yet"
        )))
    }

    pub async fn retrieve(
        &self,
        _ids: Vec<String>,
    ) -> Result<Vec<(String, Option<core_storage::document_store::StoredDocument>)>, CorelamoError>
    {
        // needs ShardManager::get_document -> route by shard_index_for, then DocumentReader
        Self::unimplemented("retrieve")
    }

    pub async fn delete(
        &self,
        _ids: Vec<String>,
    ) -> Result<core_storage::search_database::DeleteReport, CorelamoError> {
        // needs ShardCmd::Delete + ShardManager::delete with group_by_shard
        Self::unimplemented("delete")
    }

    pub async fn replace(
        &self,
        _docs: Vec<DocumentInput>,
    ) -> Result<core_storage::search_database::ReplaceReport, CorelamoError> {
        Self::unimplemented("replace")
    }

    pub async fn upsert(
        &self,
        _docs: Vec<DocumentInput>,
    ) -> Result<Vec<(usize, String, Result<(), CorelamoError>)>, CorelamoError> {
        Self::unimplemented("upsert")
    }

    pub async fn clear(&self) -> Result<(), CorelamoError> {
        Self::unimplemented("clear")
    }

    pub async fn get_logs(&self, _date: Option<String>) -> Result<String, CorelamoError> {
        // shards each have their own logger; needs a merge strategy
        Self::unimplemented("get_logs")
    }

    pub async fn clear_logs(&self) -> Result<(), CorelamoError> {
        Self::unimplemented("clear_logs")
    }

    pub async fn reindex(&self) -> Result<(), CorelamoError> {
        Self::unimplemented("reindex")
    }

    pub async fn start(
        &self,
    ) -> Result<core_storage::search_database::DatabasePowerButtonOutcome, CorelamoError> {
        // shards start on spawn; restarting means respawning the threads
        Self::unimplemented("start")
    }

    pub async fn stop(
        &self,
    ) -> Result<core_storage::search_database::DatabasePowerButtonOutcome, CorelamoError> {
        Self::unimplemented("stop")
    }

    pub async fn restart(&self) -> Result<(), CorelamoError> {
        Self::unimplemented("restart")
    }

    // pub async fn lookup(
    //     &self,
    //     cmd: LookupCommand,
    // ) -> Result<(Vec<(String, BTreeMap<String, String>)>, Vec<String>), CorelamoError> {
    //     // lookup_handler is commented out in handlers.rs too
    // }

    // pub async fn stats(&self) -> Result<DatabaseStats, CorelamoError> {
    //     // needs per-shard stats aggregation
    // }
}