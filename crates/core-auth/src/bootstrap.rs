use crate::{
    Permission::self,
    PolicyStore,
};

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
            Permission::GetLogs,
            Permission::ClearLogs,
            Permission::Lookup,
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
            Permission::GetConfig,
            Permission::SetConfig,
            Permission::Reindex,
        ],
    );

    policy.grant_many("viewer", [Permission::Search, Permission::Retrieve]);

    policy.grant_many(
        "editor",
        [
            Permission::Search,
            Permission::Retrieve,
            Permission::Insert,
            Permission::Delete,
            Permission::Status,
            Permission::Upsert,
            Permission::Replace,
            Permission::ChangeID,
            Permission::PostPolicy,
            Permission::GetPolicy,
            Permission::GetConfig,
            Permission::SetConfig,
            Permission::Reindex,
            Permission::StartDB,
            Permission::StopDB,
            Permission::RestartDB,
            Permission::Status,
        ],
    );
    policy
}
