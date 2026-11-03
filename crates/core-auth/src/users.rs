use std::io;
use std::sync::OnceLock;

use core_storage::document_store::StoredDocument;
use core_storage::json_parse::ParsedNode;
use simd_json::{json, prelude::*, OwnedValue};

use argon2::password_hash::{rand_core::OsRng, SaltString};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use core_index::{analyzer::analyzer::Analyzer, lsm::LsmIndex};
use core_storage::{document_store::DocumentStore, search_database::SearchDatabase};
// Adjust these three to wherever your movies code imports them from:
use core_protocol::format::Format;
use core_storage::search_database::{DocumentInput, IndexMode};

use crate::Principal;

pub struct UserDatabase<S: DocumentStore> {
    db: SearchDatabase<S>,
}

impl<S: DocumentStore> UserDatabase<S> {
    pub fn new(store: S, index: LsmIndex, analyzer: Analyzer) -> Self {
        Self {
            db: SearchDatabase::new(store, index, analyzer).expect("Failed to create UserDatabase"),
        }
    }

    // Shared write path so add/update don't duplicate document construction
    fn put_user(&mut self, username: &str, hash: String, roles: &[String]) -> io::Result<()> {
        let value = json!({
            "password": hash,
            "roles": roles,
        });
        let source = simd_json::to_vec(&value)
            .map_err(|e| io::Error::other(format!("failed to serialize user document: {e}")))?;

        let input = DocumentInput {
            external_id: username.to_string(),
            parsed: ParsedNode::default(),
            source,
            format: Format::JSON,
        };
        self.db.put_document(input, IndexMode::StoreOnly)?;
        
        Ok(())
    }
    /// True when no user has been created yet: a fresh installation.
    pub fn is_empty(&self) -> bool {
        self.db.document_count() == 0
    }

    pub fn add_user(
        &mut self,
        username: &str,
        password: &str,
        roles: Vec<String>,
    ) -> io::Result<()> {
        let salt = SaltString::generate(&mut OsRng);
        let hashed = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|_| io::Error::other("password hashing failed"))?
            .to_string();
        self.put_user(username, hashed, &roles)
    }

    pub fn remove_user(&mut self, username: &str) -> bool {
        match self.db.get_document(username) {
            Ok(Some(_)) => self.db.delete_document(username).is_ok(),
            _ => false,
        }
    }

    //new no-fields logic - we need to parse to read the values
    fn parse_user(doc: &StoredDocument) -> Option<(String, Vec<String>)> {
        let mut buf = doc.source.to_vec();
        let value: OwnedValue = simd_json::to_owned_value(&mut buf).ok()?;
        let obj = value.as_object()?;
        let password = obj.get("password")?.as_str()?.to_string();
        let roles = match obj.get("roles") {
            Some(OwnedValue::Array(items)) => items
                .iter()
                .filter_map(|v| v.as_str())
                .map(String::from)
                .collect(),
            _ => Vec::new(),
        };
        Some((password, roles))
    }

    pub fn update_password(&mut self, username: &str, new_password: &str) -> bool {
        let Ok(Some(doc)) = self.db.get_document(username) else {
            return false;
        };

        let Some((_, roles)) = Self::parse_user(&doc) else {
            return false;
        };

        let salt = SaltString::generate(&mut OsRng);
        let hashed = match Argon2::default().hash_password(new_password.as_bytes(), &salt) {
            Ok(hash) => hash.to_string(),
            Err(_) => {
                return false;
            }
        };
        self.put_user(username, hashed, &roles).is_ok()
    }

    pub fn update_roles(&mut self, username: &str, new_roles: Vec<String>) -> bool {
        let Ok(Some(doc)) = self.db.get_document(username) else {
            return false;
        };
        let Some((hash, _)) = Self::parse_user(&doc) else {
            return false;
        };
        self.put_user(username, hash, &new_roles).is_ok()
    }
    /// A hash of a random password, built once with the same Argon2 settings as real users.
/// Checked when the user does not exist, so both cases cost one Argon2 verify.

    pub fn verify(&self, username: &str, password: &str) -> Option<Principal> {
    let user = self
        .db
        .get_document(username)
        .ok()
        .flatten()
        .and_then(|doc| Self::parse_user(&doc));

    let (stored_hash, roles) = match user {
        Some(user) => user,
        None => {
            // same work as a real check, so the time does not reveal whether the user exists
            if let Ok(parsed) = PasswordHash::new(dummy_hash()) {
                let _ = Argon2::default().verify_password(password.as_bytes(), &parsed);
            }
            return None;
        }
    };

    let parsed_hash = PasswordHash::new(&stored_hash).ok()?;
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .ok()?;

    let mut principal = Principal::new(username);
    for role in roles {
        principal = principal.with_role(role);
    }
    Some(principal)
}

    pub fn load_principal(&self, username: &str) -> Option<Principal> {
        let doc = self.db.get_document(username).ok()??;
        let (_, roles) = Self::parse_user(&doc)?;

        let mut principal = Principal::new(username);
        for role in roles {
            principal = principal.with_role(role);
        }
        Some(principal)
    }
    pub fn user_exists(&mut self, username: &str) -> bool {
        matches!(self.db.get_document(username), Ok(Some(_)))
    }
    pub fn all_usernames(&self) -> Vec<String> {
        self.db
            .store()
            .all_documents()
            .map(|docs| docs.into_iter().map(|d| d.external_id).collect())
            .unwrap_or_default()
    }
    pub fn all_users_with_roles(&self) -> Vec<(String, Vec<String>)> {
        self.all_usernames()
            .into_iter()
            .filter_map(|username| {
                let principal = self.load_principal(&username)?;
                Some((username, principal.roles.into_iter().collect()))
            })
            .collect()
    }
}
fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(b"corelamo-no-such-user", &salt)
            .expect("hashing a constant password cannot fail")
            .to_string()
    })
}