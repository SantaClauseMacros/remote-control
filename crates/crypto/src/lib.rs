//! Device identity and (later milestones) session key agreement.
//!
//! This crate is **platform-neutral**: it creates and parses key material but
//! never touches the OS keystore. Callers are responsible for sealing the bytes
//! from [`DeviceIdentity::to_seed`] at rest — on Windows the host wraps them
//! with DPAPI before writing to disk.

use ed25519_dalek::{SigningKey, VerifyingKey, SECRET_KEY_LENGTH};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256, Sha512};
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret as XStaticSecret};
use zeroize::Zeroizing;

/// Length of the raw seed persisted for a device identity (32 bytes).
pub const IDENTITY_SEED_LEN: usize = SECRET_KEY_LENGTH;

/// A long-lived Ed25519 identity for a single installation of the host or a
/// single client device. The public key's fingerprint *is* the device ID that
/// peers pin during pairing.
pub struct DeviceIdentity {
    signing: SigningKey,
}

impl DeviceIdentity {
    /// Generate a fresh identity from the operating system CSPRNG.
    pub fn generate() -> Self {
        Self {
            signing: SigningKey::generate(&mut OsRng),
        }
    }

    /// Restore from the 32-byte seed produced by [`Self::to_seed`].
    pub fn from_seed(seed: &[u8]) -> anyhow::Result<Self> {
        let arr: [u8; SECRET_KEY_LENGTH] = seed
            .try_into()
            .map_err(|_| anyhow::anyhow!("identity seed must be {SECRET_KEY_LENGTH} bytes"))?;
        Ok(Self {
            signing: SigningKey::from_bytes(&arr),
        })
    }

    /// The 32-byte secret seed. Treat as secret; seal before persisting.
    /// Wrapped in [`Zeroizing`] so it is wiped from memory on drop.
    pub fn to_seed(&self) -> Zeroizing<[u8; SECRET_KEY_LENGTH]> {
        Zeroizing::new(self.signing.to_bytes())
    }

    pub fn public_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    /// This installation's static **X25519** secret, derived deterministically
    /// from the Ed25519 seed (SHA-512, low 32 bytes — the standard mapping).
    /// This is the private key for the Noise `XX` handshake used to authenticate
    /// a paired device on every reconnect. Handle as a secret.
    pub fn x25519_secret(&self) -> Zeroizing<[u8; 32]> {
        let seed = self.signing.to_bytes();
        let mut h = Sha512::new();
        h.update(b"remote-control/x25519/v1\0");
        h.update(seed);
        let out = h.finalize();
        let mut sk = [0u8; 32];
        sk.copy_from_slice(&out[..32]);
        Zeroizing::new(sk)
    }

    /// The matching X25519 public key — what a peer sees as our static key after
    /// a Noise handshake, and what the other side pins / allowlists.
    pub fn x25519_public(&self) -> [u8; 32] {
        XPublicKey::from(&XStaticSecret::from(*self.x25519_secret())).to_bytes()
    }

    /// Short human ID of an arbitrary X25519 public key (for the paired-device
    /// list): `XXXXX-XXXXX-XXXXX-X`, same shape as [`Self::device_id`].
    pub fn key_short_id(pubkey: &[u8; 32]) -> String {
        let mut h = Sha256::new();
        h.update(b"remote-control/key-id/v1\0");
        h.update(pubkey);
        let fp = h.finalize();
        let raw = data_encoding::BASE32_NOPAD.encode(&fp[..10]);
        raw.as_bytes()
            .chunks(5)
            .map(|c| std::str::from_utf8(c).expect("base32 is ascii"))
            .collect::<Vec<_>>()
            .join("-")
    }

    /// Full SHA-256 fingerprint of the public key.
    pub fn fingerprint(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(self.public_key().as_bytes());
        h.finalize().into()
    }

    /// Short, human-readable device ID: RFC 4648 base32 of the first 10
    /// fingerprint bytes, grouped as `XXXXX-XXXXX-XXXXX-X`.
    ///
    /// 80 bits is comfortably collision-resistant for a personal device list
    /// while staying short enough to read aloud.
    pub fn device_id(&self) -> String {
        let fp = self.fingerprint();
        let raw = data_encoding::BASE32_NOPAD.encode(&fp[..10]);
        raw.as_bytes()
            .chunks(5)
            .map(|c| std::str::from_utf8(c).expect("base32 is ascii"))
            .collect::<Vec<_>>()
            .join("-")
    }
}

/// The X25519 public key for a raw 32-byte secret, matching what a Noise peer
/// reports via `get_remote_static()` for the same secret.
pub fn x25519_public_from_secret(secret: &[u8; 32]) -> [u8; 32] {
    XPublicKey::from(&XStaticSecret::from(*secret)).to_bytes()
}

/// A random lowercase-hex token from `n_bytes` of OS randomness — for local
/// secrets such as the dashboard's per-launch access token.
pub fn random_token_hex(n_bytes: usize) -> String {
    use rand::RngCore;
    let mut bytes = vec![0u8; n_bytes];
    OsRng.fill_bytes(&mut bytes);
    data_encoding::HEXLOWER.encode(&bytes)
}

/// Six-digit numeric pairing code shown on the host and typed into the client.
pub fn random_pairing_code() -> String {
    use rand::Rng;
    let n: u32 = rand::thread_rng().gen_range(0..1_000_000);
    format!("{n:06}")
}

/// Normalise a typed device id (any case, with or without dashes) into the
/// canonical `XXXXX-XXXXX-XXXXX-X` form the host uses. Connecting only needs
/// the PC's id, which doubles as the pairing secret.
pub fn canonical_device_id(s: &str) -> String {
    let raw: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    raw.as_bytes()
        .chunks(5)
        .map(|c| std::str::from_utf8(c).expect("ascii"))
        .collect::<Vec<_>>()
        .join("-")
}

/// Derive a 32-byte pre-shared key from a pairing code, for the Noise `psk0`
/// LAN handshake.
///
/// A 6-digit code carries only ~20 bits, so a captured handshake is
/// brute-forceable offline — acceptable as a LAN-only interim. Milestone 4
/// replaces this with SPAKE2 (a PAKE: no offline brute force) plus pinned
/// per-device keys. Domain-separated so this KDF can't collide with any other
/// use of the code.
pub fn derive_pairing_psk(code: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"remote-control/lan-pairing-psk/v1\0");
    h.update(code.trim().as_bytes());
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_psk_is_stable_and_code_sensitive() {
        assert_eq!(derive_pairing_psk("482193"), derive_pairing_psk("482193"));
        assert_ne!(derive_pairing_psk("482193"), derive_pairing_psk("482194"));
        assert_eq!(derive_pairing_psk(" 000000 "), derive_pairing_psk("000000"));
        assert_eq!(random_pairing_code().len(), 6);
    }

    #[test]
    fn seed_roundtrips_and_id_is_stable() {
        let id = DeviceIdentity::generate();
        let seed = id.to_seed();
        let restored = DeviceIdentity::from_seed(seed.as_ref()).unwrap();
        assert_eq!(id.public_key().as_bytes(), restored.public_key().as_bytes());
        assert_eq!(id.device_id(), restored.device_id());
        // e.g. "ABCDE-FGHIJ-KLMNO-P"
        assert_eq!(id.device_id().len(), 16 + 3);
    }

    #[test]
    fn distinct_identities_differ() {
        assert_ne!(
            DeviceIdentity::generate().device_id(),
            DeviceIdentity::generate().device_id()
        );
    }
}
