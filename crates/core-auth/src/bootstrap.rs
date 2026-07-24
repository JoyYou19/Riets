use crate::{Permission, PolicyStore};

pub fn default_policy() -> PolicyStore {
    let mut policy = PolicyStore::new();

    policy.grant_many(
    "admin",
    [
        //basic
        Permission::Search,
        Permission::Retrieve,
        Permission::Insert,
        Permission::Delete,
        Permission::Status,
        Permission::Upsert,
        Permission::Replace,
        //databases
        Permission::CreateDatabase,
        Permission::DeleteDatabase,
        Permission::ListDatabase,
        Permission::StartDB,
        Permission::StopDB,
        Permission::RestartDB,
        //config/policy
        Permission::ChangeID,
        Permission::PostPolicy,
        Permission::GetPolicy,
        Permission::GetConfig,
        Permission::SetConfig,
        Permission::Reindex,
        //users
        Permission::CreateUser,
        Permission::DeleteUser,
        Permission::UpdatePwd,
        Permission::UpdateRole,
        
        
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

