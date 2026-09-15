//! Thin wrapper over Windows DPAPI (`CryptProtectData` / `CryptUnprotectData`).
//!
//! Used to seal secret material (device identity seeds) at rest. DPAPI ties
//! the ciphertext to the current user account, so the blob is useless if
//! copied to another machine or user — without us managing any key material
//! ourselves. Shared by the host and the desktop client so every on-disk
//! identity is sealed the same way.

use std::ffi::c_void;

use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    }
}

/// Copy `out` into a `Vec`, then release the buffer DPAPI allocated for us.
unsafe fn take_blob(out: &CRYPT_INTEGER_BLOB) -> Vec<u8> {
    let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
    let _ = LocalFree(HLOCAL(out.pbData as *mut c_void));
    v
}

/// DPAPI-encrypt `plain`, scoped to the current user.
pub fn seal(plain: &[u8]) -> Result<Vec<u8>> {
    let input = blob(plain);
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &input,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .context("CryptProtectData failed")?;
        Ok(take_blob(&out))
    }
}

/// Reverse of [`seal`].
pub fn unseal(sealed: &[u8]) -> Result<Vec<u8>> {
    let input = blob(sealed);
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .context("CryptUnprotectData failed (identity blob corrupt or from another user?)")?;
        Ok(take_blob(&out))
    }
}
