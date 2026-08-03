#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Permission {
    //basic
    Search,
    Retrieve,
    Lookup,
    Insert,
    Upsert,
    Delete,
    Status,
    Replace,
    GetLogs,
    ClearLogs,
    //databases
    CreateDatabase,
    DeleteDatabase,
    ListDatabase,
    StartDB,
    StopDB,
    RestartDB,
    //config/policy
    ChangeID,
    GetPolicy,
    PostPolicy,
    GetConfig,
    SetConfig,
    Reindex,
    //users
    CreateUser,
    DeleteUser,
    UpdateRole,
    UpdatePwd,
}

impl Permission {
    pub fn command_str(s: &str) -> Option<Self> {
        match s {
            "Search" => Some(Permission::Search),
            "Retrieve" => Some(Permission::Retrieve),
            "Insert" => Some(Permission::Insert),
            "Delete" => Some(Permission::Delete),
            "Lookup" => Some(Permission::Lookup),
            "CreateDatabase" => Some(Permission::CreateDatabase),
            "ListDatabase" => Some(Permission::ListDatabase),
            "DeleteDatabase" => Some(Permission::DeleteDatabase),
            "DeleteLogs" => Some(Permission::ClearLogs),
            "Status" => Some(Permission::Status),
            "Reindex" => Some(Permission::Reindex),
            "GetPolicy" => Some(Permission::GetPolicy),
            "PostPolicy" => Some(Permission::PostPolicy),
            "ChangeID" => Some(Permission::ChangeID),
            "CreateUser" => Some(Permission::CreateUser),
            "DeleteUser" => Some(Permission::DeleteUser),
            "UpdateRole" => Some(Permission::UpdateRole),
            "UpdatePwd" => Some(Permission::UpdatePwd),
            "Replace" => Some(Permission::Replace),
            "GetConfig" => Some(Permission::GetConfig),
            "SetConfig" => Some(Permission::SetConfig),
            "StartDB" => Some(Permission::StartDB),
            "StopDB" => Some(Permission::StopDB),
            "RestartDB" => Some(Permission::RestartDB),
            _ => None,
        }
    }
}
