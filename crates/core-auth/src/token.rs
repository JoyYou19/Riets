use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{ Digest, Sha256 };
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{ Duration, SystemTime };


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

struct TokenEntry {
    username: String,
    issued_at: SystemTime,
}
#[derive(Default)]
pub struct TokenStore {
    tokens: RwLock<HashMap<String, TokenEntry>>,
}
impl TokenStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn issue(&self, username: String) -> Token {
        let token = Token::generate();
        let hashed = hash_token(&token);
        let entry = TokenEntry {
            username,
            issued_at: SystemTime::now(),
        };
        self.tokens
            .write()
            .unwrap_or_else(
                |
                    e: std::sync::PoisonError<
                        std::sync::RwLockWriteGuard<'_, HashMap<String, TokenEntry>>
                    >
                | e.into_inner()
            )
            .insert(hashed, entry);
        token
    }
    pub fn resolve_username(&self, token: &Token) -> Option<String> {
        let hashed = hash_token(token);
        let store = self.tokens.read().unwrap_or_else(|e| e.into_inner());
        let entry = store.get(&hashed)?;
        if entry.issued_at.elapsed().ok()? > TOKEN_LIFETIME {
            return None;
        }
        Some(entry.username.clone())
    }
    pub fn revoke(&self, token: &Token) {
        let hashed = hash_token(token);
        self.tokens
            .write()
            .unwrap_or_else(
                |
                    e: std::sync::PoisonError<
                        std::sync::RwLockWriteGuard<'_, HashMap<String, TokenEntry>>
                    >
                | e.into_inner()
            )
            .remove(&hashed);
    }
    pub fn revoke_all_for(&self, username: &str) {
        self.tokens
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, entry| entry.username != username);
    }
}
