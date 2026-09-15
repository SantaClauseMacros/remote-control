//! Glue between the platform-neutral [`rc_crypto::DeviceIdentity`] and the
//! Windows DPAPI seal used to protect its seed at rest.

use anyhow::{Context, Result};
use rc_common::{dpapi, AppPaths};
use rc_crypto::DeviceIdentity;

/// Load the sealed identity from disk, or generate a new one, seal it with
/// DPAPI (user scope) and persist it atomically.
pub fn load_or_create(paths: &AppPaths) -> Result<DeviceIdentity> {
    let file = paths.identity_file();
    match std::fs::read(&file) {
        Ok(sealed) => {
            let seed = dpapi::unseal(&sealed).context("unsealing device identity")?;
            DeviceIdentity::from_seed(&seed)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let identity = DeviceIdentity::generate();
            let seed = identity.to_seed();
            let sealed = dpapi::seal(seed.as_ref()).context("sealing new device identity")?;

            let tmp = file.with_extension("bin.tmp");
            std::fs::write(&tmp, &sealed).context("writing identity temp file")?;
            std::fs::rename(&tmp, &file).context("replacing identity file")?;

            tracing::info!("generated a new device identity");
            Ok(identity)
        }
        Err(e) => Err(e).context("reading identity file"),
    }
}
