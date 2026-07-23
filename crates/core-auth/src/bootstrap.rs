use crate::{Permission, PolicyStore};

pub fn default_policy() -> PolicyStore {
    let mut policy = PolicyStore::new();

    policy.grant_many(
    "admin",
    [
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
        Permission::CreateUser,
        Permission::DeleteUser,
        Permission::UpdatePwd,
        Permission::UpdateRole,
        Permission::StartDB,
        Permission::StopDB,
        Permission::RestartDB,
    ],
    );

    policy.grant_many(
        "architect",
        [
            Permission::Search,
            Permission::Retrieve,
            Permission::CreateDatabase,
            Permission::DeleteDatabase,
            Permission::ListDatabase,
            Permission::GetPolicy,
            Permission::PostPolicy,
            Permission::Status,
        ],
    );

    policy.grant("viewer", Permission::Search);
    policy.grant("viewer", Permission::Retrieve);

    policy.grant("editor", Permission::Insert);
    policy.grant("editor", Permission::Delete);
    policy.grant("editor", Permission::Search);
    policy.grant("editor", Permission::Retrieve);
    policy.grant("editor", Permission::PostPolicy);
    policy.grant("editor", Permission::GetPolicy);
    policy.grant("editor", Permission::ChangeID);


    policy
}

