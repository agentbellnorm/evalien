//! Where connections keep what they earn at login: tokens and cookies. A
//! separate SQLite file, so the agent's `db` capability can't reach it, and
//! every value encrypted with a key that exists only in the Kubernetes Secret.

use std::{
    path::Path,
    sync::{Arc, Mutex, PoisonError},
};

use base64::Engine;
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Generate, Payload},
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::domain::Secret;

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("VAULT_KEY must be 32 bytes, base64-encoded (openssl rand -base64 32)")]
    BadKey,
    #[error("vault: {0}")]
    Storage(#[from] rusqlite::Error),
    #[error("vault: {connection}/{item} can't be decrypted: wrong key, or tampered with")]
    Undecryptable { connection: String, item: String },
}

/// The key the vault is encrypted with.
pub struct VaultKey([u8; 32]);

impl VaultKey {
    pub fn from_base64(text: &str) -> Result<Self, VaultError> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(text.trim())
            .map_err(|_| VaultError::BadKey)?;
        bytes.try_into().map(VaultKey).map_err(|_| VaultError::BadKey)
    }
}

pub struct Vault {
    db: Mutex<Connection>,
    cipher: XChaCha20Poly1305,
}

impl Vault {
    pub fn open(path: &Path, key: &VaultKey) -> Result<Self, VaultError> {
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(1))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS secrets (
                connection TEXT NOT NULL,
                item TEXT NOT NULL,
                nonce BLOB NOT NULL,
                ciphertext BLOB NOT NULL,
                PRIMARY KEY (connection, item)
            )",
        )?;
        let cipher = XChaCha20Poly1305::new_from_slice(&key.0).map_err(|_| VaultError::BadKey)?;
        Ok(Vault {
            db: Mutex::new(db),
            cipher,
        })
    }

    pub fn put(&self, connection: &str, item: &str, value: &Secret) -> Result<(), VaultError> {
        let nonce = XNonce::generate();
        let aad = associated_data(connection, item);
        let payload = Payload {
            msg: value.expose().as_bytes(),
            aad: &aad,
        };
        let ciphertext = self
            .cipher
            .encrypt(&nonce, payload)
            .map_err(|_| self.undecryptable(connection, item))?;
        self.db().execute(
            "INSERT INTO secrets (connection, item, nonce, ciphertext) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (connection, item) DO UPDATE SET nonce = excluded.nonce, ciphertext = excluded.ciphertext",
            params![connection, item, nonce.as_slice(), ciphertext],
        )?;
        Ok(())
    }

    pub fn get(&self, connection: &str, item: &str) -> Result<Option<Secret>, VaultError> {
        let row: Option<(Vec<u8>, Vec<u8>)> = self
            .db()
            .query_row(
                "SELECT nonce, ciphertext FROM secrets WHERE connection = ?1 AND item = ?2",
                params![connection, item],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((nonce, ciphertext)) = row else {
            return Ok(None);
        };
        let nonce = XNonce::try_from(nonce.as_slice()).map_err(|_| self.undecryptable(connection, item))?;
        let aad = associated_data(connection, item);
        let plaintext = self
            .cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: &ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| self.undecryptable(connection, item))?;
        let text = String::from_utf8(plaintext).map_err(|_| self.undecryptable(connection, item))?;
        Ok(Some(Secret::new(text)))
    }

    pub fn delete(&self, connection: &str, item: &str) -> Result<(), VaultError> {
        self.db().execute(
            "DELETE FROM secrets WHERE connection = ?1 AND item = ?2",
            params![connection, item],
        )?;
        Ok(())
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn undecryptable(&self, connection: &str, item: &str) -> VaultError {
        VaultError::Undecryptable {
            connection: connection.to_owned(),
            item: item.to_owned(),
        }
    }
}

/// One connection's items in the vault. A session only reaches its own.
#[derive(Clone)]
pub struct ConnectionVault {
    vault: Arc<Vault>,
    connection: String,
}

impl ConnectionVault {
    pub fn new(vault: Arc<Vault>, connection: &str) -> Self {
        ConnectionVault {
            vault,
            connection: connection.to_owned(),
        }
    }

    pub fn get(&self, item: &str) -> Result<Option<Secret>, VaultError> {
        self.vault.get(&self.connection, item)
    }

    pub fn put(&self, item: &str, value: &Secret) -> Result<(), VaultError> {
        self.vault.put(&self.connection, item, value)
    }

    pub fn delete(&self, item: &str) -> Result<(), VaultError> {
        self.vault.delete(&self.connection, item)
    }
}

/// Binds each ciphertext to its row, so values can't be swapped between rows.
fn associated_data(connection: &str, item: &str) -> Vec<u8> {
    format!("evalien-vault-v1\0{connection}\0{item}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> VaultKey {
        VaultKey::from_base64(&base64::engine::general_purpose::STANDARD.encode([byte; 32])).unwrap()
    }

    #[test]
    fn values_round_trip_encrypted_and_stay_bound_to_their_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.db");
        let vault = Vault::open(&path, &key(1)).unwrap();
        vault
            .put("google", "refresh_token", &Secret::new("1//very-secret"))
            .unwrap();
        vault
            .put("google", "refresh_token", &Secret::new("1//rotated-secret"))
            .unwrap();
        vault.put("kivra", "cookies", &Secret::new("session=abc")).unwrap();
        assert_eq!(
            vault.get("google", "refresh_token").unwrap().unwrap().expose(),
            "1//rotated-secret"
        );
        assert!(vault.get("google", "missing").unwrap().is_none());

        let file = std::fs::read(&path).unwrap();
        for value in [&b"very-secret"[..], b"rotated-secret", b"session=abc"] {
            assert!(!file.windows(value.len()).any(|w| w == value), "plaintext on disk");
        }

        // A row copied onto another doesn't decrypt there.
        vault.db().execute_batch("UPDATE secrets SET nonce = (SELECT nonce FROM secrets WHERE connection = 'kivra'), ciphertext = (SELECT ciphertext FROM secrets WHERE connection = 'kivra') WHERE connection = 'google'").unwrap();
        assert!(matches!(
            vault.get("google", "refresh_token"),
            Err(VaultError::Undecryptable { .. })
        ));

        vault.delete("kivra", "cookies").unwrap();
        assert!(vault.get("kivra", "cookies").unwrap().is_none());
    }

    #[test]
    fn the_wrong_key_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.db");
        Vault::open(&path, &key(1))
            .unwrap()
            .put("google", "token", &Secret::new("t"))
            .unwrap();
        let other = Vault::open(&path, &key(2)).unwrap();
        assert!(matches!(
            other.get("google", "token"),
            Err(VaultError::Undecryptable { .. })
        ));
    }

    #[test]
    fn keys_must_be_32_bytes_of_base64() {
        assert!(VaultKey::from_base64("not base64!").is_err());
        assert!(VaultKey::from_base64(&base64::engine::general_purpose::STANDARD.encode([0u8; 16])).is_err());
    }
}
