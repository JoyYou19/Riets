use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UserId(pub String);

#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    pub id: UserId,
    pub roles: HashSet<String>,
}

impl Principal {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: UserId(id.into()),
            roles: HashSet::new(),
        }
    }

    pub fn with_role(mut self, role: impl Into<String>) -> Self {
        self.roles.insert(role.into());
        self
    }
}
