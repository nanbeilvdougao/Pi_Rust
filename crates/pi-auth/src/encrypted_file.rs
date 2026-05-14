//! Encrypted credential file.
//!
//! ChaCha20-Poly1305 AEAD with a key derived from `SHA-256(machine_id || salt)`.
//! See module-level docs in `lib.rs` for the threat model — this is not
//! defense against a determined local attacker; it is "don't put your API key
//! in `~/.bash_history`."

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use chacha20poly1305::aead::{Aead, KeyInit, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use pi_core::{PiError, PiErrorKind, PiResult};
use sha2::{Digest, Sha256};

use crate::{CredentialFile, Resolver};

const SALT: &[u8] = b"pi-rust auth v1";
const NONCE_LEN: usize = 12;

pub struct EncryptedFileStore {
    path: PathBuf,
    key: [u8; 32],
    cache: CredentialFile,
}

impl EncryptedFileStore {
    pub fn open(path: PathBuf) -> PiResult<Self> {
        let key = derive_key()?;
        let cache = load_file(&path, &key)?;
        Ok(Self { path, key, cache })
    }

    fn save(&self) -> PiResult<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let plaintext = toml::to_string(&self.cache)
            .map_err(|err| PiError::new(PiErrorKind::Config, format!("序列化凭证失败：{err}")))?;
        let cipher = ChaCha20Poly1305::new(&self.key.into());
        let mut nonce_bytes = [0u8; NONCE_LEN];
        getrandom_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = cipher
            .encrypt(nonce, plaintext.as_bytes())
            .map_err(|err| PiError::new(PiErrorKind::Config, format!("加密失败：{err}")))?;
        let tmp_path = self.path.with_extension("enc.tmp");
        {
            let mut tmp = fs::File::create(&tmp_path)?;
            tmp.write_all(&nonce_bytes)?;
            tmp.write_all(&ciphertext)?;
            tmp.flush()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&tmp_path)?.permissions();
                perms.set_mode(0o600);
                fs::set_permissions(&tmp_path, perms)?;
            }
        }
        fs::rename(&tmp_path, &self.path)?;
        Ok(())
    }
}

fn load_file(path: &PathBuf, key: &[u8; 32]) -> PiResult<CredentialFile> {
    if !path.exists() {
        return Ok(CredentialFile::default());
    }
    let bytes = fs::read(path)?;
    if bytes.len() <= NONCE_LEN {
        return Ok(CredentialFile::default());
    }
    let (nonce_bytes, ciphertext) = bytes.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(key.into());
    let nonce = Nonce::from_slice(nonce_bytes);
    let plaintext = cipher.decrypt(nonce, ciphertext).map_err(|err| {
        PiError::new(
            PiErrorKind::Config,
            format!(
                "解密 auth 文件失败：{err}。如已不可恢复，可删除 {} 重新登录。",
                path.display()
            ),
        )
    })?;
    let text = String::from_utf8(plaintext)
        .map_err(|err| PiError::new(PiErrorKind::Config, format!("auth 文件非 UTF-8：{err}")))?;
    toml::from_str(&text)
        .map_err(|err| PiError::new(PiErrorKind::Config, format!("解析 auth 文件失败：{err}")))
}

fn derive_key() -> PiResult<[u8; 32]> {
    let machine_id = machine_id().unwrap_or_else(persisted_user_id);
    let mut hasher = Sha256::new();
    hasher.update(machine_id.as_bytes());
    hasher.update(SALT);
    let digest = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&digest);
    Ok(key)
}

fn machine_id() -> Option<String> {
    if let Ok(text) = fs::read_to_string("/etc/machine-id") {
        let id = text.trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    if let Ok(text) = fs::read_to_string("/var/lib/dbus/machine-id") {
        let id = text.trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = std::process::Command::new("ioreg")
            .args(["-rd1", "-c", "IOPlatformExpertDevice"])
            .output()
        {
            if let Ok(text) = String::from_utf8(output.stdout) {
                for line in text.lines() {
                    if let Some(idx) = line.find("\"IOPlatformUUID\" = \"") {
                        let rest = &line[idx + "\"IOPlatformUUID\" = \"".len()..];
                        if let Some(end) = rest.find('"') {
                            return Some(rest[..end].to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

/// Fallback machine identifier — a random value persisted under
/// `~/.pi-rust/.machine-id`. Same security level as `/etc/machine-id`: anyone
/// with read access can derive the key.
fn persisted_user_id() -> String {
    if let Ok(home) = std::env::var("HOME") {
        let path = PathBuf::from(home).join(".pi-rust").join(".machine-id");
        if let Ok(text) = fs::read_to_string(&path) {
            let trimmed = text.trim().to_string();
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let mut bytes = [0u8; 32];
        getrandom_bytes(&mut bytes);
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let _ = fs::write(&path, &hex);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = fs::metadata(&path) {
                let mut perms = meta.permissions();
                perms.set_mode(0o600);
                let _ = fs::set_permissions(&path, perms);
            }
        }
        return hex;
    }
    // Last resort: hostname.
    "pi-rust-fallback-machine-id".to_string()
}

fn getrandom_bytes(buf: &mut [u8]) {
    // ChaCha's OsRng wraps the platform CSPRNG; reuse it so we don't pull
    // `getrandom` directly here.
    use chacha20poly1305::aead::rand_core::RngCore;
    let mut rng = OsRng;
    rng.fill_bytes(buf);
}

impl Resolver for EncryptedFileStore {
    fn lookup(&self, _provider: &str, env_name: &str) -> PiResult<Option<String>> {
        Ok(self.cache.credentials.get(env_name).cloned())
    }

    fn store(&mut self, _provider: &str, env_name: &str, value: &str) -> PiResult<()> {
        self.cache
            .credentials
            .insert(env_name.to_string(), value.to_string());
        self.save()
    }

    fn delete(&mut self, _provider: &str, env_name: &str) -> PiResult<bool> {
        let removed = self.cache.credentials.remove(env_name).is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    fn list(&self) -> PiResult<Vec<String>> {
        Ok(self.cache.credentials.keys().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn round_trip_through_encrypted_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.enc");
        {
            let mut store = EncryptedFileStore::open(path.clone()).unwrap();
            store.store("openai", "OPENAI_API_KEY", "sk-test").unwrap();
        }
        let store = EncryptedFileStore::open(path).unwrap();
        let v = store.lookup("openai", "OPENAI_API_KEY").unwrap();
        assert_eq!(v.as_deref(), Some("sk-test"));
    }

    #[test]
    fn delete_returns_true_when_removed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.enc");
        let mut store = EncryptedFileStore::open(path).unwrap();
        store.store("x", "X_KEY", "v").unwrap();
        assert!(store.delete("x", "X_KEY").unwrap());
        assert!(!store.delete("x", "X_KEY").unwrap());
    }

    #[test]
    fn list_returns_stored_env_names() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.enc");
        let mut store = EncryptedFileStore::open(path).unwrap();
        store.store("x", "X_KEY", "v1").unwrap();
        store.store("y", "Y_KEY", "v2").unwrap();
        let names = store.list().unwrap();
        assert!(names.contains(&"X_KEY".to_string()));
        assert!(names.contains(&"Y_KEY".to_string()));
    }
}
