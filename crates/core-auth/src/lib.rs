mod bootstrap;
mod permission;
pub mod principal;
mod roles;
mod token;
mod users;

pub use bootstrap::default_policy;
use core_protocol::errors::CorelamoError;
use core_storage::binary_store::BinaryDocumentStore;
pub use permission::Permission;
pub use principal::{ Principal, UserId };
pub use roles::PolicyStore;
pub use token::{ Token, TokenStore };
pub use users::UserDatabase;

pub struct AuthService {
    policy: PolicyStore,
    tokens: TokenStore,
    users: UserDatabase<BinaryDocumentStore>,
}

impl AuthService {
    pub fn new(
        policy: PolicyStore,
        tokens: TokenStore,
        users: UserDatabase<BinaryDocumentStore>
    ) -> Self {
        Self {
            policy,
            tokens,
            users,
        }
    }
    pub fn bootstrap(mut users: UserDatabase<BinaryDocumentStore>) -> Self {
        let policy = default_policy();
        let _ = users.add_user("admin", "secret", vec!["admin".to_string()]);
        Self::new(policy, TokenStore::new(), users)
    }
    pub fn create_user(
        &mut self,
        requester: &Principal,
        username: &str,
        password: &str,
        roles: Vec<String>
    ) -> Result<(), CorelamoError> {
        self.check(requester, Permission::CreateUser)?;

        if self.users.user_exists(username) {
            return Err(CorelamoError::AlreadyExists(format!("user '{username}' already exists")));
        }
        if let Some(bad) = roles.iter().find(|r| !self.policy.has_role(r)) {
            return Err(CorelamoError::InvalidData(format!("unknown role '{bad}'")));
        }
        self.users
            .add_user(username, password, roles)
            .map_err(|e| {
                CorelamoError::Internal(format!("failed to create user '{username}': {e}"))
            })?;
        Ok(())
    }
    pub fn delete_user(
        &mut self,
        requester: &Principal,
        username: &str
    ) -> Result<(), CorelamoError> {
        self.check(requester, Permission::DeleteUser)?;
        if self.users.remove_user(username) {
            Ok(())
        } else {
            Err(CorelamoError::NotFound(format!("user '{}' not found", username)))
        }
    }
    pub fn update_user_password(
        &mut self,
        requester: &Principal,
        username: &str,
        new_password: &str
    ) -> Result<(), CorelamoError> {
        let is_self = requester.id.0 == username;
        if !is_self {
            self.check(requester, Permission::UpdatePwd)?;
        }
        if self.users.update_password(username, new_password) {
            Ok(())
        } else {
            Err(CorelamoError::NotFound(format!("user '{}' not found", username)))
        }
    }
    pub fn update_user_roles(
        &mut self,
        requester: &Principal,
        username: &str,
        new_roles: Vec<String>
    ) -> Result<(), CorelamoError> {
        self.check(requester, Permission::UpdateRole)?;
        if self.users.update_roles(username, new_roles) {
            Ok(())
        } else {
            Err(CorelamoError::NotFound(format!("user '{}'not found ", username)))
        }
    }

    pub fn login(&mut self, username: &str, password: &str) -> Option<Token> {
        let _ = self.users.verify(username, password)?;
        Some(self.tokens.issue(username.to_string()))
    }
    pub fn authenticate(&self, token: &Token) -> Option<Principal> {
        let username = self.tokens.resolve_username(token)?;
        self.users.load_principal(&username)
    }

    pub fn check(
        &self,
        principal: &Principal,
        permission: Permission
    ) -> Result<(), CorelamoError> {
        let allowed = principal.roles
            .iter()
            .any(|role| self.policy.role_has_permission(role, &permission));

        if allowed {
            Ok(())
        } else {
            Err(
                CorelamoError::PermissionDenied(
                    format!("user '{}' lacks permission '{:?}'", principal.id.0, permission)
                )
            )
        }
    }
    pub fn list_usernames(&self) -> Vec<String> {
        self.users.all_usernames() 
    }
}
