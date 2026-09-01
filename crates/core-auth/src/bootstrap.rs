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
            Permission::AllFields,
            //databases
            Permission::CreateDatabase,
            Permission::DeleteDatabase,
            Permission::ListDatabases,
            Permission::RenameDatabase,
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
            Permission::ListUsers,
            //backup
            Permission::BackupFull,
            Permission::Restore,
            Permission::BackupIncremental,
            Permission::ListBackups,
            Permission::Backup,
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
            Permission::RenameDatabase,
            Permission::StartDB,
            Permission::StopDB,
            Permission::RestartDB,
            Permission::GetPolicy,
            Permission::SetPolicy,
            Permission::GetConfig,
            Permission::SetConfig,
            Permission::Reindex,
            Permission::AllFields         
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
            Permission::AllFields,
        ],
    );

    policy
}
