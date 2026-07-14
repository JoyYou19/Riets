use std::collections::HashMap;
use argon2::{Argon2, PasswordHasher, PasswordVerifier, PasswordHash};
use argon2::password_hash::{SaltString, rand_core::OsRng};

use crate::principal::Principal;

pub struct UserStore {
    users: HashMap<String, (String, Vec<String>)>, // username -> (hashed_password, roles)
}

impl UserStore {
    pub fn new() -> Self {
        Self { users: HashMap::new() }
    }
}

impl Default for UserStore {
    fn default() -> Self {
        Self::new()
    }
}
impl UserStore {
    pub fn add_user(&mut self, username: &str, password: &str, roles: Vec<String>) {
        let salt = SaltString::generate(&mut OsRng);
        let hashed = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .unwrap()
            .to_string();
        self.users.insert(username.to_string(), (hashed, roles));
    }

    pub fn verify(&self, username: &str, password: &str) -> Option<Principal> {
        let (stored_hash, roles) = self.users.get(username)?;
        let parsed_hash = PasswordHash::new(stored_hash).ok()?;

        Argon2::default()
            .verify_password(password.as_bytes(), &parsed_hash)
            .ok()?;

        let mut principal = Principal::new(username);
        for role in roles {
            principal = principal.with_role(role.clone());
        }
        Some(principal)
    }
}