//! Integration credentials: OS keyring, with encrypted file fallback.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use color_eyre::eyre::{Context, eyre};
use keyring::Entry;
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::persist;

/// Keyring service name for this application.
pub const SERVICE: &str = "tod";

/// Account name for the Linear API key.
pub const LINEAR_ACCOUNT: &str = "linear";

/// Optional non-persisted override (CI / one-off).
pub const ENV_LINEAR_API_KEY: &str = "TOD_LINEAR_API_KEY";

const CREDENTIALS_SUBDIR: &str = "credentials";
const LINEAR_FILE_NAME: &str = "linear_api_key";
const FILE_VERSION: u8 = 1;
const NONCE_LEN: usize = 12;
const KEY_SALT: &[u8] = b"tod-linear-v1";

/// Where a credential was persisted.
#[derive(Debug, Clone)]
pub enum CredentialStore {
    Keyring,
    EncryptedFile { path: PathBuf },
}

impl CredentialStore {
    /// Status line describing where the secret lives.
    pub fn status_message(&self) -> String {
        match self {
            Self::Keyring => "Linear API key saved in OS keyring".to_string(),
            Self::EncryptedFile { path } => format!(
                "OS keyring unavailable; saved encrypted key to {}",
                path.display()
            ),
        }
    }
}

fn linear_entry() -> color_eyre::Result<Entry> {
    Entry::new(SERVICE, LINEAR_ACCOUNT).wrap_err("creating keyring entry for Linear")
}

fn is_keyring_unavailable(err: &keyring::Error) -> bool {
    matches!(
        err,
        keyring::Error::PlatformFailure(_) | keyring::Error::NoStorageAccess(_)
    )
}

/// Path to the encrypted Linear API key file: `{config}/credentials/linear_api_key`.
pub fn linear_api_key_file_path() -> color_eyre::Result<PathBuf> {
    Ok(persist::config_dir()?
        .join(CREDENTIALS_SUBDIR)
        .join(LINEAR_FILE_NAME))
}

fn ensure_credentials_dir() -> color_eyre::Result<PathBuf> {
    let dir = persist::config_dir()?.join(CREDENTIALS_SUBDIR);
    fs::create_dir_all(&dir).wrap_err_with(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

fn machine_material() -> Vec<u8> {
    if let Ok(id) = fs::read_to_string("/etc/machine-id") {
        let trimmed = id.trim();
        if !trimmed.is_empty() {
            return trimmed.as_bytes().to_vec();
        }
    }
    let host = fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown-host".to_string());
    let uid = std::env::var("UID").unwrap_or_else(|_| "0".into());
    format!("{host}:{uid}").into_bytes()
}

fn derive_file_key() -> Key {
    let mut hasher = Sha256::new();
    hasher.update(KEY_SALT);
    hasher.update(machine_material());
    let digest = hasher.finalize();
    *Key::from_slice(&digest)
}

fn encrypt_secret(plaintext: &str) -> color_eyre::Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(&derive_file_key());
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|err| eyre!("encrypting credential: {err}"))?;
    let mut out = Vec::with_capacity(1 + NONCE_LEN + ciphertext.len());
    out.push(FILE_VERSION);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

fn decrypt_secret(blob: &[u8]) -> color_eyre::Result<String> {
    if blob.len() < 1 + NONCE_LEN + 16 {
        return Err(eyre!("credential file too short"));
    }
    if blob[0] != FILE_VERSION {
        return Err(eyre!("unsupported credential file version {}", blob[0]));
    }
    let nonce = Nonce::from_slice(&blob[1..1 + NONCE_LEN]);
    let ciphertext = &blob[1 + NONCE_LEN..];
    let cipher = ChaCha20Poly1305::new(&derive_file_key());
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| eyre!("decrypting credential file failed (wrong machine or corrupt file)"))?;
    String::from_utf8(plaintext).wrap_err("credential plaintext was not UTF-8")
}

#[derive(Debug)]
enum FileLoad {
    Missing,
    Found(String),
    /// File exists but cannot be decrypted (wrong machine, corrupt, bad version).
    Unreadable,
}

fn load_from_file() -> color_eyre::Result<FileLoad> {
    let path = linear_api_key_file_path()?;
    if !path.exists() {
        return Ok(FileLoad::Missing);
    }
    let blob = fs::read(&path).wrap_err_with(|| format!("reading {}", path.display()))?;
    match decrypt_secret(&blob) {
        Ok(secret) if secret.is_empty() => Ok(FileLoad::Missing),
        Ok(secret) => Ok(FileLoad::Found(secret)),
        Err(_) => Ok(FileLoad::Unreadable),
    }
}

fn store_to_file(api_key: &str) -> color_eyre::Result<PathBuf> {
    ensure_credentials_dir()?;
    let path = linear_api_key_file_path()?;
    let blob = encrypt_secret(api_key)?;
    write_private_file(&path, &blob)?;
    Ok(path)
}

fn write_private_file(path: &Path, data: &[u8]) -> color_eyre::Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .wrap_err_with(|| format!("opening {}", path.display()))?;
    file.write_all(data)
        .wrap_err_with(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(path, perms)
            .wrap_err_with(|| format!("setting permissions on {}", path.display()))?;
    }
    Ok(())
}

fn delete_file_fallback() {
    let _ = remove_linear_api_key_file();
}

/// Remove the encrypted Linear API key fallback file if it exists.
pub fn remove_linear_api_key_file() -> color_eyre::Result<()> {
    let path = linear_api_key_file_path()?;
    if path.exists() {
        fs::remove_file(&path).wrap_err_with(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

fn load_from_keyring() -> color_eyre::Result<Option<String>> {
    let entry = match linear_entry() {
        Ok(entry) => entry,
        Err(err) => {
            // Treat construction failures like unavailable storage.
            let msg = format!("{err:#}");
            if msg.contains("Platform") || msg.contains("permission") {
                return Ok(None);
            }
            return Err(err);
        }
    };
    match entry.get_password() {
        Ok(password) if !password.is_empty() => Ok(Some(password)),
        Ok(_) => Ok(None),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) if is_keyring_unavailable(&err) => Ok(None),
        Err(err) => Err(eyre!("reading Linear API key from keyring: {err}")),
    }
}

fn store_to_keyring(api_key: &str) -> color_eyre::Result<()> {
    let entry = linear_entry()?;
    entry
        .set_password(api_key)
        .map_err(|err| eyre!("storing Linear API key in keyring: {err}"))
}

/// Result of trying to load the Linear API key from env / keyring / file.
#[derive(Debug)]
pub enum LoadLinearApiKey {
    Found(String),
    Missing,
    /// Encrypted fallback exists but cannot be decrypted (e.g. new machine or corrupt file).
    UnreadableFile {
        path: PathBuf,
    },
}

/// Load the Linear API key: env → OS keyring → encrypted config file.
pub fn load_linear_api_key() -> color_eyre::Result<LoadLinearApiKey> {
    if let Ok(key) = std::env::var(ENV_LINEAR_API_KEY) {
        let trimmed = key.trim();
        if !trimmed.is_empty() {
            return Ok(LoadLinearApiKey::Found(trimmed.to_string()));
        }
    }

    if let Some(key) = load_from_keyring()? {
        return Ok(LoadLinearApiKey::Found(key));
    }

    match load_from_file()? {
        FileLoad::Found(key) => Ok(LoadLinearApiKey::Found(key)),
        FileLoad::Missing => Ok(LoadLinearApiKey::Missing),
        FileLoad::Unreadable => Ok(LoadLinearApiKey::UnreadableFile {
            path: linear_api_key_file_path()?,
        }),
    }
}

/// Store the Linear API key in the OS keyring, or encrypted file if keyring fails.
pub fn store_linear_api_key(api_key: &str) -> color_eyre::Result<CredentialStore> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err(eyre!("Linear API key cannot be empty"));
    }

    match store_to_keyring(api_key) {
        Ok(()) => {
            delete_file_fallback();
            Ok(CredentialStore::Keyring)
        }
        Err(keyring_err) => {
            // Fall back for any keyring store failure (common in containers).
            let path = store_to_file(api_key).wrap_err_with(|| {
                format!(
                    "OS keyring failed ({keyring_err:#}) and encrypted file fallback also failed"
                )
            })?;
            Ok(CredentialStore::EncryptedFile { path })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let secret = "lin_api_test_secret";
        let blob = encrypt_secret(secret).expect("encrypt");
        assert_eq!(blob[0], FILE_VERSION);
        let out = decrypt_secret(&blob).expect("decrypt");
        assert_eq!(out, secret);
    }

    #[test]
    fn private_file_roundtrip_explicit_path() {
        let mut dir = std::env::temp_dir();
        let n: u64 = rand::random();
        dir.push(format!("tod-cred-test-{n}"));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("linear_api_key");
        let blob = encrypt_secret("lin_file_secret").expect("encrypt");
        write_private_file(&path, &blob).expect("write");
        let read = fs::read(&path).expect("read");
        let loaded = decrypt_secret(&read).expect("decrypt");
        assert_eq!(loaded, "lin_file_secret");
        let _ = fs::remove_dir_all(&dir);
    }
}
