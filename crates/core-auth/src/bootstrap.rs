use crate::{AuthService, PolicyStore, TokenStore, UserStore, Permission};

pub fn default_auth_service() -> AuthService {
    let mut policy = PolicyStore::new();
    policy.grant_many("admin",[
        Permission::Search,
        Permission::Retrieve,
        Permission::Insert,
        Permission::Delete,
        Permission::CreateDatabase,
        Permission::DeleteDatabase,
        Permission::ListDatabase,
        Permission::ChangeID,
        Permission::PostPolicy,
        Permission::GetPolicy,
        Permission::Status,
    ]);
    

    let mut users = UserStore::new();
    users.add_user("admin", "secret", vec!["admin".to_string()]);

    AuthService::new(policy, TokenStore::new(), users)
}