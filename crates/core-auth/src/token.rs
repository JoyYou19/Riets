use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, SystemTime};

use crate::Principal;

//Can change to whatever
const TOKEN_LIFETIME: Duration = Duration::from_secs(86400);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Token(pub String);
impl Token {
    fn generate() -> Token {
        let mut bytes = [0u8; 32]; // 256 bits of entropy
        OsRng.fill_bytes(&mut bytes);
        Token(hex::encode(bytes)) // or base64, either is fine
    }
}
fn hash_token(token: &Token) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.0.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub struct TokenEntry {
    pub principal: Principal,
    pub issued_at: SystemTime,
}
#[derive(Default)]
pub struct TokenStore {
    tokens: RwLock<HashMap<String, TokenEntry>>,
}
impl TokenStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn issue(&self, principal: Principal) -> Token {
        let token = Token::generate();
        let hashed = hash_token(&token);
        let entry = TokenEntry {
            principal,
            issued_at: SystemTime::now(),
        };
        self.tokens
            .write()
            .unwrap_or_else(
                |e: std::sync::PoisonError<
                    std::sync::RwLockWriteGuard<'_, HashMap<String, TokenEntry>>,
                >| e.into_inner(),
            )
            .insert(hashed, entry);
        token
    }
    pub fn resolve(&self, token: &Token) -> Option<Principal> {
        let hashed = hash_token(token);
        let store = self.tokens.read().unwrap_or_else(
            |e: std::sync::PoisonError<
                std::sync::RwLockReadGuard<'_, HashMap<String, TokenEntry>>,
            >| e.into_inner(),
        );
        let entry = store.get(&hashed)?;
        if entry.issued_at.elapsed().ok()? > TOKEN_LIFETIME {
            return None;
        }
        Some(entry.principal.clone())
    }
    pub fn revoke(&self, token: &Token) {
        let hashed = hash_token(token);
        self.tokens
            .write()
            .unwrap_or_else(
                |e: std::sync::PoisonError<
                    std::sync::RwLockWriteGuard<'_, HashMap<String, TokenEntry>>,
                >| e.into_inner(),
            )
            .remove(&hashed);
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_token_resolves_to_correct_principal() {
        let store = TokenStore::new();
        let principal = Principal::new("admin").with_role("admin");

        let token = store.issue(principal.clone());
        let resolved = store.resolve(&token);

        assert!(resolved.is_some());
    }

    #[test]
    fn invalid_token_does_not_resolve() {
        let store = TokenStore::new();
        let fake_token = Token("not-a-real-token".to_string());

        assert!(store.resolve(&fake_token).is_none());
    }

    #[test]
    fn revoked_token_no_longer_resolves() {
        let store = TokenStore::new();
        let principal = Principal::new("bob");

        let token = store.issue(principal);
        assert!(store.resolve(&token).is_some());

        store.revoke(&token);
        assert!(store.resolve(&token).is_none());
    }

    #[test]
    fn two_tokens_for_same_user_are_different() {
        let store = TokenStore::new();
        let principal = Principal::new("carol");

        let token_a = store.issue(principal.clone());
        let token_b = store.issue(principal);

        assert_ne!(token_a.0, token_b.0);
    }
}
