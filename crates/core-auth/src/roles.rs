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

    pub fn grant_many(
        &mut self,
        role: impl Into<String>,
        permissions: impl IntoIterator<Item = Permission>,
    ) {
        let role = role.into();
        let entry = self.role_permissions.entry(role).or_default();
        for permission in permissions {
            entry.insert(permission);
        }
    }
    pub fn has_role(&self, role: &str) -> bool {
        self.role_permissions.contains_key(role)
    }

    pub fn role_has_permission(&self, role: &str, permission: &Permission) -> bool {
        self.role_permissions
            .get(role)
            .is_some_and(|perms| perms.contains(permission))
    }

    pub fn remove(&mut self, role: impl Into<String>, permission: Permission) {
        let role = role.into();
        if let Some(perms) = self.role_permissions.get_mut(&role) {
            perms.remove(&permission);
        }
    }
}
