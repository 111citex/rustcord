//! Where the account token lives: the OS keyring (Secret Service on Linux,
//! Credential Manager on Windows). Never a plain file, never a log line.

use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
#[error("couldn't use the system keyring: {0}")]
pub struct TokenStoreError(String);

pub trait TokenStore: Send + Sync {
    fn load(&self) -> Option<String>;
    fn save(&self, token: &str) -> Result<(), TokenStoreError>;
    fn clear(&self);
}

pub struct KeyringStore {
    user: String,
}

impl KeyringStore {
    pub fn new() -> Self {
        Self::with_user("token")
    }

    pub fn with_user(user: &str) -> Self {
        KeyringStore { user: user.to_string() }
    }

    fn entry(&self) -> Result<keyring::Entry, TokenStoreError> {
        keyring::Entry::new("rustcord", &self.user).map_err(|e| TokenStoreError(e.to_string()))
    }
}

impl Default for KeyringStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenStore for KeyringStore {
    fn load(&self) -> Option<String> {
        match self.entry().ok()?.get_password() {
            Ok(token) => Some(token),
            Err(keyring::Error::NoEntry) => None,
            Err(error) => {
                tracing::warn!(%error, "couldn't read the saved session from the keyring");
                None
            }
        }
    }

    fn save(&self, token: &str) -> Result<(), TokenStoreError> {
        self.entry()?.set_password(token).map_err(|e| TokenStoreError(e.to_string()))
    }

    fn clear(&self) {
        if let Ok(entry) = self.entry() {
            let _ = entry.delete_credential();
        }
    }
}

#[derive(Default)]
pub struct MemoryStore(Mutex<Option<String>>);

impl TokenStore for MemoryStore {
    fn load(&self) -> Option<String> {
        self.0.lock().unwrap().clone()
    }

    fn save(&self, token: &str) -> Result<(), TokenStoreError> {
        *self.0.lock().unwrap() = Some(token.to_string());
        Ok(())
    }

    fn clear(&self) {
        *self.0.lock().unwrap() = None;
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trip() {
        let s = MemoryStore::default();
        assert_eq!(s.load(), None);
        s.save("abc").unwrap();
        assert_eq!(s.load().as_deref(), Some("abc"));
        s.clear();
        assert_eq!(s.load(), None);
    }

    /// Touches the real OS keyring. Run by hand: `cargo test -p cord-core keyring_round_trip -- --ignored`
    #[test]
    #[ignore]
    fn keyring_round_trip() {
        let s = KeyringStore::with_user("test-token");
        s.save("not-a-real-token").unwrap();
        assert_eq!(s.load().as_deref(), Some("not-a-real-token"));
        s.clear();
        assert_eq!(s.load(), None);
    }
}
