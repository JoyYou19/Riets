mod bootstrap;
mod error;
mod permission;
mod principal;
mod roles;
mod token;
mod users;

pub use bootstrap::default_auth_service;
pub use error::AuthError;
pub use permission::Permission;
pub use principal::{Principal, UserId};
pub use roles::PolicyStore;
pub use token::{Token, TokenStore};
pub use users::UserStore;

pub struct AuthService {
    policy: PolicyStore,
    tokens: TokenStore,
    users: UserStore,
}

impl AuthService {
    pub fn new(policy: PolicyStore, tokens: TokenStore, users: UserStore) -> Self {
        Self {
            policy,
            tokens,
            users,
        }
    }

    pub fn login(&self, username: &str, password: &str) -> Option<Token> {
        let principal = self.users.verify(username, password)?;
        Some(self.tokens.issue(principal))
    }
    pub fn authenticate(&self, token: &Token) -> Option<Principal> {
        self.tokens.resolve(token)
    }

    pub fn check(&self, principal: &Principal, permission: Permission) -> Result<(), AuthError> {
        let allowed = principal
            .roles
            .iter()
            .any(|role| self.policy.role_has_permission(role, &permission));

        if allowed {
            Ok(())
        } else {
            Err(AuthError::PermissionDenied {
                user: principal.id.0.clone(),
                permission: format!("{:?}", permission),
            })
        }
    }
}
