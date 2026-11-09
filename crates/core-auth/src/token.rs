use core_protocol::errors::CorelamoError;
use rand::rngs::SysRng;
use rand::TryRng;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, SystemTime};

//Can change to whatever
const TOKEN_LIFETIME: Duration = Duration::from_hours(24);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Token(pub String);
impl Token {
    /// 256 bits from the operating system's RNG, hex encoded. The OS RNG can fail (rand 0.10
    /// made it fallible), and a login must then fail too rather than panic.
    fn generate() -> Result<Token, CorelamoError> {
        let mut bytes = [0u8; 32];
        SysRng
            .try_fill_bytes(&mut bytes)
            .map_err(|e| CorelamoError::Internal(format!("token generation failed: {e}")))?;
        Ok(Token(hex::encode(bytes)))
    }
}
fn hash_token(token: &Token) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.0.as_bytes());
    hex::encode(hasher.finalize())
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
    pub fn issue(&self, username: String) -> Result<Token, CorelamoError> {
        let token = Token::generate()?;
        let hashed = hash_token(&token);
        let mut store = self
            .tokens
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // expired tokens are never used again: drop them here, so the map holds only live ones
        store.retain(|_, entry| {
            entry
                .issued_at
                .elapsed()
                .is_ok_and(|age| age <= TOKEN_LIFETIME)
        });
        store.insert(
            hashed,
            TokenEntry {
                username,
                issued_at: SystemTime::now(),
            },
        );
        Ok(token)
    }
    pub fn resolve_username(&self, token: &Token) -> Option<String> {
        let hashed = hash_token(token);
        let store = self
            .tokens
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = store.get(&hashed)?;
        if entry.issued_at.elapsed().ok()? > TOKEN_LIFETIME {
            return None;
        }
        Some(entry.username.clone())
    }
}

