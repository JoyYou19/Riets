use std::collections::BTreeMap;
use std::io;

use argon2::password_hash::{rand_core::OsRng, SaltString};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use core_index::{analyzer::analyzer::Analyzer, lsm::LsmIndex};
use core_storage::{document_store::DocumentStore, search_database::SearchDatabase};
// Adjust these three to wherever your movies code imports them from:
use core_protocol::format::Format;
use core_storage::search_database::{DocumentInput, IndexMode};

use crate::Principal ;

pub struct UserDatabase<S: DocumentStore> {
    db: SearchDatabase<S>,

}

impl<S: DocumentStore> UserDatabase<S> {
    pub fn new(store: S, index: LsmIndex, analyzer: Analyzer) -> Self {
        Self {
            db: SearchDatabase::new(store, index, analyzer),
   
        }
    }

    // Shared write path so add/update don't duplicate document construction
    fn put_user(&mut self, username: &str, hash: String, roles: &[String]) -> io::Result<()> {
        let mut fields = BTreeMap::new();
        fields.insert("password".to_string(), hash);
        
        fields.insert("roles".to_string(), roles.join(","));

        let input = DocumentInput {
            external_id: username.to_string(),
            fields,
            source: vec![],
            format: Format::JSON,
        };
        self.db.put_document(input, IndexMode::StoreOnly)?;
        Ok(())
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

    pub fn update_password(&mut self, username: &str, new_password: &str) -> bool {
        let Ok(Some(doc)) = self.db.get_document(username) else {
            return false;
        };
        let Some(roles_str) = doc.fields.get("roles") else {
            return false;
        };
        let roles: Vec<String> = roles_str.split(',').map(String::from).collect();

        let salt = SaltString::generate(&mut OsRng);
        let hashed = match Argon2::default().hash_password(new_password.as_bytes(), &salt) {
            Ok(hash) => hash.to_string(),
            Err(_) => return false,
        };
        self.put_user(username, hashed, &roles).is_ok()
    }

    pub fn update_roles(&mut self, username: &str, new_roles: Vec<String>) -> bool {
        let Ok(Some(doc)) = self.db.get_document(username) else {
            return false;
        };
        let Some(hash) = doc.fields.get("password").cloned() else {
            return false;
        };
        self.put_user(username, hash, &new_roles).is_ok()
    }

    pub fn verify(&mut self, username: &str, password: &str) -> Option<Principal> {
        let doc = self.db.get_document(username).ok()??;
        let stored_hash = doc.fields.get("password")?;
        let roles: Vec<String> = doc
            .fields
            .get("roles")?
            .split(',')
            .map(String::from)
            .collect();

        let parsed_hash = PasswordHash::new(stored_hash).ok()?;
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed_hash)
            .ok()?;

        let mut principal = Principal::new(username);
        for role in roles {
            principal = principal.with_role(role);
        }
        Some(principal)
    }
    pub fn load_principal(&mut self, username: &str) -> Option<Principal> {
    let doc = self.db.get_document(username).ok()??;
    let roles: Vec<String> = doc
        .fields
        .get("roles")?
        .split(',')
        .map(String::from)
        .collect();

    let mut principal = Principal::new(username);
    for role in roles {
        principal = principal.with_role(role);
    }
    Some(principal)
    }   
    pub fn user_exists(&mut self, username: &str) -> bool {
    matches!(self.db.get_document(username), Ok(Some(_)))
    }
}
