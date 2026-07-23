

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Permission {
    Search,
    Retrieve,
    Insert,
    Upsert,
    Delete,
    CreateDatabase,
    ListDatabase,
    DeleteDatabase,
    Status,
    Reindex,
    GetPolicy,
    PostPolicy,
    ChangeID,
    CreateUser,
    DeleteUser,
    UpdateRole,
    UpdatePwd,
    Replace,
    GetConfig,
    SetConfig,
    StartDB,
    StopDB,
    RestartDB,
    
    

}

impl Permission {
    pub fn command_str(s: &str) -> Option<Self> {
        match s {
            "Search" => Some(Permission::Search),
            "Retrieve" => Some(Permission::Retrieve),
            "Insert" => Some(Permission::Insert),
            "Delete" => Some(Permission::Delete),
            "CreateDatabase" => Some(Permission::CreateDatabase),
            "ListDatabase" => Some(Permission::ListDatabase),
            "DeleteDatabase" => Some(Permission::DeleteDatabase),
            "Status" =>Some(Permission::Status),
            "Reindex" =>Some(Permission::Reindex),
            "GetPolicy" =>Some(Permission::GetPolicy),
            "PostPolicy" => Some(Permission::PostPolicy),
            "ChangeID" => Some(Permission::ChangeID),
            "CreateUser" =>Some(Permission::CreateUser),
            "DeleteUser" =>Some(Permission::DeleteUser),
            "UpdateRole" =>Some(Permission::UpdateRole),
            "UpdatePwd" =>Some(Permission::UpdatePwd),
            "Replace" => Some(Permission::Replace),
            "GetConfig" =>Some(Permission::GetConfig),
            "SetConfig" =>Some(Permission::SetConfig),
            "StartDB" =>Some(Permission::StartDB),
            "StopDB" => Some(Permission::StopDB),
            "RestartDB" =>Some(Permission::RestartDB),
            _ => None,
        }
    }
}