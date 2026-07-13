#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Permission {
    Read,
    Write,
    Delete,
    Admin,
    Modify,
    Create,

}

impl Permission {
    pub fn command_str(s: &str) -> Option<Self> {
        match s {
            "read" => Some(Permission::Read),
            "write" => Some(Permission::Write),
            "delete" => Some(Permission::Delete),
            "admin" => Some(Permission::Admin),
            "modify" => Some(Permission::Modify),
            "create" => Some(Permission::Create),

            _ => None,
        }
    }
}