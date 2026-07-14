

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Permission {
    Search,
    Retrieve,
    Insert,
    Delete,
    CreateDatabase,
    ListDatabase,
    DeleteDatabase,
    Status,
    Reindex,
    GetPolicy,
    PostPolicy,
    ChangeID,


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

            _ => None,
        }
    }
}