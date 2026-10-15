//One slot per database, each owning its ShardManager.
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use core_core::shard_manager::ShardManager;
use core_protocol::errors::CorelamoError;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock as Gate};

use crate::database_helpers::{colliding_dir, normalize_db_name};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    Creating,
    Live,
    Deleting,
    Renaming,
}

struct Inner {
    state: SlotState,
    manager: Option<Arc<ShardManager>>,
}

pub struct DbSlot {
    pub name: String,
    pub dir: PathBuf,
    //dont be scared Valters, this lock is only for the lifecycle opperations of the databse
    inner: RwLock<Inner>,
    pub gate: Arc<Gate<()>>,
}

impl DbSlot {
    fn with_state(
        name: String,
        dir: PathBuf,
        state: SlotState,
        manager: Option<Arc<ShardManager>>,
    ) -> Self {
        Self {
            name,
            dir,
            inner: RwLock::new(Inner { state, manager }),
            gate: Arc::new(Gate::new(())),
        }
    }

    pub fn live(name: String, dir: PathBuf, manager: Arc<ShardManager>) -> Self {
        Self::with_state(name, dir, SlotState::Live, Some(manager))
    }

    pub fn state(&self) -> SlotState {
        self.inner_read().state
    }

    /// Read access to this database's gate: held by document and search work for as long as
    /// it touches the database. Owned, so it can outlive the caller's `Arc<DbSlot>`.
    pub async fn read(&self) -> OwnedRwLockReadGuard<()> {
        Arc::clone(&self.gate).read_owned().await
    }

    /// Write access: held by lifecycle, configuration and restore work.
    pub async fn write(&self) -> OwnedRwLockWriteGuard<()> {
        Arc::clone(&self.gate).write_owned().await
    }

    pub fn manager(&self) -> Result<Arc<ShardManager>, CorelamoError> {
        let inner = self.inner_read();
        match inner.state {
            SlotState::Live => inner.manager.as_ref().map(Arc::clone).ok_or_else(|| {
                CorelamoError::Internal(format!("database '{}' has no manager", self.name))
            }),
            SlotState::Creating => Err(CorelamoError::Conflict(format!(
                "database '{}' is being created",
                self.name
            ))),
            SlotState::Deleting => Err(CorelamoError::Conflict(format!(
                "database '{}' is being deleted",
                self.name
            ))),
            SlotState::Renaming => Err(CorelamoError::Conflict(format!(
                "database '{}' is being renamed",
                self.name
            ))),
        }
    }

    pub fn set_live(&self, manager: Arc<ShardManager>) {
        let mut inner = self.inner_write();
        inner.manager = Some(manager);
        inner.state = SlotState::Live;
    }

    pub fn set_state(&self, state: SlotState) {
        self.inner_write().state = state;
    }

    fn inner_read(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn inner_write(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct Databases {
    dir: PathBuf,
    slots: RwLock<HashMap<String, Arc<DbSlot>>>,
}

impl Databases {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            slots: RwLock::new(HashMap::new()),
        }
    }

    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    pub fn get(&self, name: &str) -> Result<Arc<DbSlot>, CorelamoError> {
        let name = normalize_db_name(name);
        self.read()
            .get(&name)
            .cloned()
            .ok_or_else(|| CorelamoError::NotFound(format!("database '{name}' not found")))
    }

    pub fn list(&self) -> Vec<Arc<DbSlot>> {
        self.read().values().cloned().collect()
    }

    pub fn remove(&self, name: &str) -> Option<Arc<DbSlot>> {
        self.write().remove(&normalize_db_name(name))
    }

    pub fn reserve(&self, name: &str) -> Result<Arc<DbSlot>, CorelamoError> {
        let name = normalize_db_name(name);
        let mut slots = self.write();
        if slots.contains_key(&name) {
            return Err(CorelamoError::AlreadyExists(format!(
                "database '{name}' already exists"
            )));
        }
        if let Some(other) = colliding_dir(&self.dir, &name) {
            return Err(CorelamoError::AlreadyExists(format!(
                "database '{name}' already exists on disk as '{other}'"
            )));
        }
        let slot = Arc::new(DbSlot::with_state(
            name.clone(),
            self.dir.join(&name),
            SlotState::Creating,
            None,
        ));
        slots.insert(name, Arc::clone(&slot));
        Ok(slot)
    }

    /// Puts a slot in without reserving: used at boot, and by a rename for its new name.
    pub fn insert(&self, slot: Arc<DbSlot>) -> Result<(), CorelamoError> {
        let mut slots = self.write();
        if slots.contains_key(&slot.name) {
            return Err(CorelamoError::AlreadyExists(format!(
                "database '{}' already exists",
                slot.name
            )));
        }
        slots.insert(slot.name.clone(), slot);
        Ok(())
    }

    fn read(&self) -> RwLockReadGuard<'_, HashMap<String, Arc<DbSlot>>> {
        self.slots.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, HashMap<String, Arc<DbSlot>>> {
        self.slots.write().unwrap_or_else(|e| e.into_inner())
    }
}
