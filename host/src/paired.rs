//! The host's allow-list of paired client devices.
//!
//! A device is added the first time it connects with a valid pairing code (and
//! the user confirms, if that's on). After that it reconnects with no code —
//! authentication is the pinned static key. Removing an entry revokes it.
//!
//! The file holds only public keys, so it isn't sealed; it's still written
//! atomically so a crash can't corrupt it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PairedDevice {
    /// Friendly label (client-reported or user-set later).
    pub label: String,
    /// Unix seconds.
    pub added: u64,
    pub last_seen: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    /// base64(url-safe, no pad) of the 32-byte X25519 key → record.
    devices: BTreeMap<String, PairedDevice>,
}

pub struct PairedStore {
    path: PathBuf,
    file: File,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn key_str(key: &[u8; 32]) -> String {
    data_encoding::BASE64URL_NOPAD.encode(key)
}

impl PairedStore {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("paired.json");
        let file = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { path, file }
    }

    pub fn count(&self) -> usize {
        self.file.devices.len()
    }

    pub fn is_paired(&self, key: &[u8; 32]) -> bool {
        self.file.devices.contains_key(&key_str(key))
    }

    /// Add (or refresh) a device after a successful first pairing.
    pub fn upsert(&mut self, key: &[u8; 32], label: &str) {
        let k = key_str(key);
        let entry = self.file.devices.entry(k).or_insert_with(|| PairedDevice {
            label: label.to_string(),
            added: now(),
            last_seen: 0,
        });
        if !label.is_empty() {
            entry.label = label.to_string();
        }
        entry.last_seen = now();
        self.save();
    }

    /// Note that a paired device just reconnected.
    pub fn touch(&mut self, key: &[u8; 32]) {
        if let Some(e) = self.file.devices.get_mut(&key_str(key)) {
            e.last_seen = now();
            self.save();
        }
    }

    pub fn forget_all(&mut self) {
        self.file.devices.clear();
        self.save();
    }

    fn save(&self) {
        let Ok(json) = serde_json::to_vec_pretty(&self.file) else {
            return;
        };
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}
