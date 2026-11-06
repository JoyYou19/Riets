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
use rand::{ TryRng, rngs::SysRng };
pub use roles::PolicyStore;
pub use token::{ Token, TokenStore };
pub use users::UserDatabase;

use crate::users::ListUsers;
pub const SUPERUSER:&str = "corelamo";
pub const MIN_PASSWORD_LEN:usize=8;
pub fn generate_password() -> Result<String, CorelamoError> {
    let mut bytes = [0u8; 16];
    SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| CorelamoError::Internal(format!("generating a password: {e}")))?;
    Ok(hex::encode(bytes))
}
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
   pub fn bootstrap(
        mut users: UserDatabase<BinaryDocumentStore>,
    ) -> Result<(Self, Option<String>), CorelamoError> {
        let mut created = None;
        if users.is_empty() {
            let password = generate_password()?;
            users
                .add_user(SUPERUSER, &password, vec!["admin".to_string()])
                .map_err(|e| CorelamoError::Internal(format!("creating '{SUPERUSER}': {e}")))?;
            created = Some(password);
        }
        Ok((Self::new(default_policy(), TokenStore::new(), users), created))
    }
    pub fn set_initial_password(&mut self, password: &str) -> Result<(), CorelamoError> {
        if self.users.update_password(SUPERUSER, password) {
            Ok(())
        } else {
            Err(CorelamoError::Internal(format!(
                "could not set the password of '{SUPERUSER}'"
            )))
        }
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
            Err(CorelamoError::NotFound(format!("user '{username}' not found")))
        }
    }
    pub fn update_user_password(
        &mut self,
        requester: &Principal,
        username: &str,
        new_password: &str,
    ) -> Result<(), CorelamoError> {
        let is_self = requester.id.0 == username;
        if !is_self {
            self.check(requester, Permission::UpdatePwd)?;
        }
        if new_password.chars().count() < MIN_PASSWORD_LEN {
            return Err(CorelamoError::InvalidData(format!(
                "password must be at least {MIN_PASSWORD_LEN} characters"
            )));
        }
        if self.users.update_password(username, new_password) {
            Ok(())
        } else {
            Err(CorelamoError::NotFound(format!(
                "user '{username}' not found"
            )))
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
            Err(CorelamoError::NotFound(format!("user '{username}'not found ")))
        }
    }

    /// `Ok(None)` means wrong credentials; `Err` means the token could not be issued.
    pub fn login(&self, username: &str, password: &str) -> Result<Option<Token>, CorelamoError> {
        if self.users.verify(username, password).is_none() {
            return Ok(None);
        }
        self.tokens.issue(username.to_string()).map(Some)
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
    pub fn list_users(&self) -> Result<Vec<ListUsers>, CorelamoError> {
        self.users
            .all_users_with_roles()
            .map_err(|e| CorelamoError::Internal(format!("reading the user store: {e}")))
    }
}
