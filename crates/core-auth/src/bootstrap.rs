use crate::{
    Permission::{self},
    PolicyStore,
};

pub fn default_policy() -> PolicyStore {
    let mut policy = PolicyStore::new();

    //all
    policy.grant_many(
        "admin",
        [
            //basic
            Permission::Search,
            Permission::Retrieve,
            Permission::Lookup,
            Permission::Insert,
            Permission::Upsert,
            Permission::Delete,
            Permission::Status,
            Permission::Replace,
            Permission::ClearDB,
            Permission::GetLogs,
            Permission::ClearLogs,
            //databases
            Permission::CreateDatabase,
            Permission::DeleteDatabase,
            Permission::ListDatabases,
            Permission::StartDB,
            Permission::StopDB,
            Permission::RestartDB,
            //config/policy
            Permission::GetPolicy,
            Permission::SetPolicy,
            Permission::GetConfig,
            Permission::SetConfig,
            Permission::Reindex,
            //users
            Permission::CreateUser,
            Permission::DeleteUser,
            Permission::UpdatePwd,
            Permission::UpdateRole,
            //backup
            Permission::BackupFull,
            Permission::Restore,
            Permission::BackupIncremental,
        ],
    );

    //admin kip
    policy.grant_many(
        "architect",
        [
            Permission::Search,
            Permission::Retrieve,
            Permission::Lookup,
            Permission::Status,
            Permission::CreateDatabase,
            Permission::DeleteDatabase,
            Permission::ListDatabases,
            Permission::StartDB,
            Permission::StopDB,
            Permission::RestartDB,
            Permission::GetPolicy,
            Permission::SetPolicy,
            Permission::GetConfig,
            Permission::SetConfig,
            Permission::Reindex,
        ],
    );

    // read-only
    policy.grant_many(
        "viewer",
        [Permission::Search, Permission::Retrieve, Permission::Lookup],
    );

    // read/write on data only
    policy.grant_many(
        "editor",
        [
            Permission::Search,
            Permission::Retrieve,
            Permission::Lookup,
            Permission::Insert,
            Permission::Upsert,
            Permission::Delete,
            Permission::Replace,
            Permission::Status,
        ],
    );

    policy
}
