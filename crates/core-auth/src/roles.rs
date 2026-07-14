use std::collections::HashMap;
use std::collections::HashSet;

use crate::permission::Permission;


#[derive(Default)]
pub struct PolicyStore {
    role_permissions: HashMap<String, HashSet<Permission>>,
}

impl PolicyStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn grant(&mut self, role: impl Into<String>, permission: Permission) {
        self.role_permissions
            .entry(role.into())
            .or_default()
            .insert(permission);
    }
    pub fn grant_many(&mut self, role: impl Into<String>, permissions: impl IntoIterator<Item = Permission>) {
    let role = role.into();
    let entry = self.role_permissions.entry(role).or_default();
    for permission in permissions {
        entry.insert(permission);
    }
}
    pub fn role_has_permission(&self, role: &str, permission: &Permission) -> bool {
        self.role_permissions
            .get(role)
            .map(|perms| perms.contains(permission))
            .unwrap_or(false)
    }
    pub fn remove(&mut self, role: impl Into<String>, permission: Permission) {
    let role = role.into();
    if let Some(perms) = self.role_permissions.get_mut(&role) {
        perms.remove(&permission);
    }
}

}


