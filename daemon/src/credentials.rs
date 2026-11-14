//! USB port secrets, deliberately separate from settings and IPC snapshots.
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::{Mutex, atomic::{AtomicU64, Ordering}};

use anyhow::{bail, Result};
use serde_json::{Map, Value, json};

type Passwords = BTreeMap<String, BTreeMap<String, String>>;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub struct CredentialsStore {
    path: PathBuf,
    passwords: Mutex<Passwords>,
}

impl CredentialsStore {
    pub fn new(path: PathBuf) -> Self {
        let passwords = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                tracing::error!("USB password file is invalid; saved passwords unavailable");
                Passwords::new()
            }),
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::error!("USB password file cannot be read; saved passwords unavailable");
                }
                Passwords::new()
            }
        };
        Self { path, passwords: Mutex::new(passwords) }
    }

    pub fn password(&self, mac: &str, port: &str) -> Option<Vec<u8>> {
        self.passwords.lock().unwrap().get(&mac.to_ascii_lowercase())?
            .get(port).map(|p| p.as_bytes().to_vec())
    }

    pub fn status(&self, mac: &str) -> Value {
        json!({"usb3": self.password(mac, "usb3").is_some(),
               "usb2": self.password(mac, "usb2").is_some()})
    }

    /// Omitted port means unchanged; an empty string removes its saved password.
    pub fn update(&self, mac: &str, updates: &Map<String, Value>) -> Result<()> {
        for (port, value) in updates {
            if !matches!(port.as_str(), "usb2" | "usb3") { bail!("unknown USB port"); }
            let password = value.as_str().ok_or_else(|| anyhow::anyhow!("USB password must be a string"))?;
            if password.len() > 16 || password.contains('\0') {
                bail!("USB password must contain at most 16 UTF-8 bytes and no NUL");
            }
        }
        let mut guard = self.passwords.lock().unwrap();
        let mut next = guard.clone();
        let key = mac.to_ascii_lowercase();
        let ports = next.entry(key.clone()).or_default();
        for (port, value) in updates {
            let password = value.as_str().unwrap();
            if password.is_empty() { ports.remove(port); }
            else { ports.insert(port.clone(), password.to_string()); }
        }
        if ports.is_empty() { next.remove(&key); }
        if let Some(parent) = self.path.parent() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        }
        let tmp = self.path.with_extension(format!("tmp-{}-{}", std::process::id(), TEMP_ID.fetch_add(1, Ordering::Relaxed)));
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        let result = (|| -> Result<()> {
            file.write_all(&serde_json::to_vec(&next)?)?;
            file.sync_all()?;
            std::fs::rename(&tmp, &self.path)?;
            Ok(())
        })();
        if result.is_err() { let _ = std::fs::remove_file(&tmp); }
        result?;
        *guard = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn secrets_are_port_and_server_specific_private_and_persistent() {
        let path = std::env::temp_dir().join(format!("osl-password-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CredentialsStore::new(path.clone());
        store.update("AA", json!({"usb2":"two", "usb3":"three"}).as_object().unwrap()).unwrap();
        assert_eq!(store.password("aa", "usb2"), Some(b"two".to_vec()));
        assert_eq!(store.password("AA", "usb3"), Some(b"three".to_vec()));
        assert_eq!(store.password("BB", "usb3"), None);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let reloaded = CredentialsStore::new(path.clone());
        assert_eq!(reloaded.password("aa", "usb3"), Some(b"three".to_vec()));
        assert!(reloaded.update("aa", json!({"usb2":"updated", "usb3":"ééééééééé"}).as_object().unwrap()).is_err());
        assert_eq!(reloaded.password("aa", "usb2"), Some(b"two".to_vec()));
        assert!(reloaded.update("aa", json!({"usb2":"a\u{0000}b"}).as_object().unwrap()).is_err());
        reloaded.update("aa", json!({"usb2":""}).as_object().unwrap()).unwrap();
        assert_eq!(reloaded.password("aa", "usb2"), None);
        assert_eq!(reloaded.password("aa", "usb3"), Some(b"three".to_vec()));
        assert_eq!(reloaded.status("aa"), json!({"usb2":false,"usb3":true}));
        std::fs::remove_file(path).unwrap();
    }
}
