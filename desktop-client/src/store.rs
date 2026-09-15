//! Per-machine identity + pinned host keys for the desktop client.
//!
//! Lives under `%LOCALAPPDATA%\RemoteControl\desktop-client\`. The identity
//! seed is DPAPI-sealed at rest (user scope), the same way the host seals
//! its own — a copy of the file is useless off this machine and this user.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use rc_common::dpapi;
use rc_crypto::DeviceIdentity;

fn dir() -> Result<PathBuf> {
    let base = directories::ProjectDirs::from("dev", "RemoteControl", "RemoteControl")
        .context("no profile dir")?
        .data_local_dir()
        .join("desktop-client");
    std::fs::create_dir_all(&base)?;
    Ok(base)
}

/// This client's static X25519 secret (32 bytes), generated on first run.
pub fn load_identity() -> Result<[u8; 32]> {
    let path = dir()?.join("identity.bin");
    let id = match std::fs::read(&path) {
        Ok(sealed) => {
            let seed = dpapi::unseal(&sealed).context("unsealing client identity")?;
            DeviceIdentity::from_seed(&seed)?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let id = DeviceIdentity::generate();
            let sealed = dpapi::seal(id.to_seed().as_ref()).context("sealing new client identity")?;
            let tmp = path.with_extension("bin.tmp");
            std::fs::write(&tmp, &sealed)?;
            std::fs::rename(&tmp, &path)?;
            id
        }
        Err(e) => return Err(e).context("reading client identity"),
    };
    Ok(*id.x25519_secret())
}

/// `target key (device id or address) → base64url host X25519 key`.
pub struct PinnedHosts {
    path: PathBuf,
    map: BTreeMap<String, String>,
}

impl PinnedHosts {
    pub fn load() -> Self {
        let path = dir().map(|d| d.join("hosts.json")).unwrap_or_default();
        let map = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { path, map }
    }

    pub fn get(&self, key: &str) -> Option<[u8; 32]> {
        let s = self.map.get(key)?;
        let v = data_encoding::BASE64URL_NOPAD.decode(s.as_bytes()).ok()?;
        v.try_into().ok()
    }

    pub fn set(&mut self, key: &str, host_key: &[u8; 32]) {
        self.map.insert(
            key.to_string(),
            data_encoding::BASE64URL_NOPAD.encode(host_key),
        );
        if let Ok(j) = serde_json::to_vec_pretty(&self.map) {
            let tmp = self.path.with_extension("json.tmp");
            if std::fs::write(&tmp, j).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
    }
}
