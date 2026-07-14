#[derive(Debug)]
pub enum AuthError {
    PermissionDenied { user: String, permission: String },
    UnknownRole(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::PermissionDenied { user, permission } => {
                write!(f, "user '{}' lacks permission '{}'", user, permission)
            }
            AuthError::UnknownRole(role) => write!(f, "unknown role '{}'", role),
        }
    }
}

impl std::error::Error for AuthError {}