//! Space keys, device box keys, and object encryption at rest.
//!
//! Object ids stay `BLAKE3(plaintext)`. Ciphertext is what a mailbox stores.
//! A device's X25519 box key wraps space keys; a recovery secret wraps them
//! again so a new device can unwrap without a surviving peer.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use base64ct::{Base64, Encoding};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ed25519_dalek::pkcs8::DecodePrivateKey;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::RngCore;
use relay_core::{DeviceId, ObjectId};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::CryptoError;

const BOX_FILE: &str = "box.key";
const OBJECT_MAGIC: &[u8] = b"RLY1";
const RECOVERY_MAGIC: &[u8] = b"RRK1";
const WRAP_MAGIC: &[u8] = b"RWK1";

/// X25519 key used only to wrap space keys. Not the Ed25519 device identity.
pub struct BoxKeyPair {
    secret: [u8; 32],
    public: [u8; 32],
}

impl std::fmt::Debug for BoxKeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxKeyPair")
            .field("public", &hex::encode(self.public))
            .finish_non_exhaustive()
    }
}

impl BoxKeyPair {
    fn generate(dir: &Path) -> Result<Self, CryptoError> {
        let path = dir.join(BOX_FILE);
        if path.exists() {
            return Err(CryptoError::KeyExists(path));
        }
        fs::create_dir_all(dir).map_err(io_err(dir))?;
        let secret = random_bytes();
        write_private(&path, &secret)?;
        Ok(Self::from_secret(secret))
    }

    pub fn load(dir: &Path) -> Result<Self, CryptoError> {
        let path = dir.join(BOX_FILE);
        let bytes = fs::read(&path).map_err(io_err(&path))?;
        let secret: [u8; 32] = bytes.try_into().map_err(|_| CryptoError::InvalidKey {
            path,
            message: "box key is not 32 bytes".into(),
        })?;
        Ok(Self::from_secret(secret))
    }

    pub fn load_or_generate(dir: &Path) -> Result<Self, CryptoError> {
        let path = dir.join(BOX_FILE);
        if path.is_file() {
            return Self::load(dir);
        }
        match Self::generate(dir) {
            Ok(key) => Ok(key),
            Err(CryptoError::KeyExists(_)) => Self::load(dir),
            Err(err) => Err(err),
        }
    }

    fn from_secret(secret: [u8; 32]) -> Self {
        let public = PublicKey::from(&StaticSecret::from(secret)).to_bytes();
        Self { secret, public }
    }

    pub fn public_bytes(&self) -> [u8; 32] {
        self.public
    }

    pub fn unwrap(&self, wrapped: &[u8]) -> Result<[u8; 32], CryptoError> {
        unwrap_key(&self.secret, wrapped)
    }
}

pub fn random_bytes() -> [u8; 32] {
    let mut out = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut out);
    out
}

/// Anonymous wrap of a 32-byte space key to an X25519 public key.
pub fn wrap_key(recipient_public: &[u8; 32], space_key: &[u8; 32]) -> Result<Vec<u8>, CryptoError> {
    let ephemeral = random_bytes();
    let eph_secret = StaticSecret::from(ephemeral);
    let eph_public = PublicKey::from(&eph_secret);
    let shared = eph_secret.diffie_hellman(&PublicKey::from(*recipient_public));
    let kek = blake3::derive_key("relay-space-wrap-v1", shared.as_bytes());
    let nonce = random_nonce();
    let cipher = XChaCha20Poly1305::new_from_slice(&kek).map_err(|_| CryptoError::Wrap)?;
    let ct = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: space_key,
                aad: WRAP_MAGIC,
            },
        )
        .map_err(|_| CryptoError::Wrap)?;
    let mut out = Vec::with_capacity(4 + 32 + 24 + ct.len());
    out.extend_from_slice(WRAP_MAGIC);
    out.extend_from_slice(eph_public.as_bytes());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub(crate) fn unwrap_key(
    recipient_secret: &[u8; 32],
    wrapped: &[u8],
) -> Result<[u8; 32], CryptoError> {
    if wrapped.len() < 4 + 32 + 24 + 16 || &wrapped[..4] != WRAP_MAGIC {
        return Err(CryptoError::Wrap);
    }
    let mut eph = [0u8; 32];
    eph.copy_from_slice(&wrapped[4..36]);
    let eph_public = PublicKey::from(eph);
    let nonce: [u8; 24] = wrapped[36..60].try_into().map_err(|_| CryptoError::Wrap)?;
    let ct = &wrapped[60..];
    let shared = StaticSecret::from(*recipient_secret).diffie_hellman(&eph_public);
    let kek = blake3::derive_key("relay-space-wrap-v1", shared.as_bytes());
    let cipher = XChaCha20Poly1305::new_from_slice(&kek).map_err(|_| CryptoError::Wrap)?;
    let plain = cipher
        .decrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: ct,
                aad: WRAP_MAGIC,
            },
        )
        .map_err(|_| CryptoError::Wrap)?;
    plain.try_into().map_err(|_| CryptoError::Wrap)
}

/// Seal plaintext for `id` under a space key. The generation is stored in the header.
pub fn seal_object(
    space_key: &[u8; 32],
    generation: u32,
    id: &ObjectId,
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let nonce = random_nonce();
    let cipher = XChaCha20Poly1305::new_from_slice(space_key).map_err(|_| CryptoError::Seal)?;
    let aad = object_aad(generation, id);
    let ct = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::Seal)?;
    let mut out = Vec::with_capacity(4 + 4 + 24 + ct.len());
    out.extend_from_slice(OBJECT_MAGIC);
    out.extend_from_slice(&generation.to_le_bytes());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn sealed_generation(sealed: &[u8]) -> Option<u32> {
    if sealed.len() < 8 || &sealed[..4] != OBJECT_MAGIC {
        return None;
    }
    Some(u32::from_le_bytes(sealed[4..8].try_into().ok()?))
}

pub fn open_object(
    space_key: &[u8; 32],
    id: &ObjectId,
    sealed: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let generation = sealed_generation(sealed).ok_or(CryptoError::Seal)?;
    if sealed.len() < 8 + 24 + 16 {
        return Err(CryptoError::Seal);
    }
    let nonce: [u8; 24] = sealed[8..32].try_into().map_err(|_| CryptoError::Seal)?;
    let ct = &sealed[32..];
    let cipher = XChaCha20Poly1305::new_from_slice(space_key).map_err(|_| CryptoError::Seal)?;
    let aad = object_aad(generation, id);
    cipher
        .decrypt(XNonce::from_slice(&nonce), Payload { msg: ct, aad: &aad })
        .map_err(|_| CryptoError::Seal)
}

pub fn seal_recovery(
    recovery_secret: &[u8; 32],
    space_key: &[u8; 32],
) -> Result<Vec<u8>, CryptoError> {
    let kek = blake3::derive_key("relay-recovery-v1", recovery_secret);
    let nonce = random_nonce();
    let cipher = XChaCha20Poly1305::new_from_slice(&kek).map_err(|_| CryptoError::Seal)?;
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), space_key.as_slice())
        .map_err(|_| CryptoError::Seal)?;
    let mut out = Vec::with_capacity(4 + 24 + ct.len());
    out.extend_from_slice(RECOVERY_MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn open_recovery(recovery_secret: &[u8; 32], sealed: &[u8]) -> Result<[u8; 32], CryptoError> {
    if sealed.len() < 4 + 24 + 16 || &sealed[..4] != RECOVERY_MAGIC {
        return Err(CryptoError::Seal);
    }
    let nonce: [u8; 24] = sealed[4..28].try_into().map_err(|_| CryptoError::Seal)?;
    let kek = blake3::derive_key("relay-recovery-v1", recovery_secret);
    let cipher = XChaCha20Poly1305::new_from_slice(&kek).map_err(|_| CryptoError::Seal)?;
    let plain = cipher
        .decrypt(XNonce::from_slice(&nonce), &sealed[28..])
        .map_err(|_| CryptoError::Seal)?;
    plain.try_into().map_err(|_| CryptoError::Seal)
}

pub(crate) fn sign_pem(pem: &str, message: &[u8]) -> Result<[u8; 64], CryptoError> {
    let b64: String = pem
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('-'))
        .collect();
    let der = Base64::decode_vec(&b64).map_err(|e| CryptoError::InvalidKey {
        path: PathBuf::from("device.key"),
        message: e.to_string(),
    })?;
    let key = SigningKey::from_pkcs8_der(&der).map_err(|e| CryptoError::InvalidKey {
        path: PathBuf::from("device.key"),
        message: e.to_string(),
    })?;
    Ok(key.sign(message).to_bytes())
}

pub fn verify_device(device: &DeviceId, message: &[u8], signature: &[u8]) -> bool {
    let Ok(bytes) = <[u8; 64]>::try_from(signature) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(device.as_bytes()) else {
        return false;
    };
    key.verify(message, &Signature::from_bytes(&bytes)).is_ok()
}

/// Hex groups of 4, so a recovery secret can be written down.
pub fn format_recovery(secret: &[u8; 32]) -> String {
    let hex = hex::encode(secret);
    (0..hex.len())
        .step_by(4)
        .map(|i| &hex[i..i + 4])
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn parse_recovery(text: &str) -> Result<[u8; 32], CryptoError> {
    let hex_str: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = hex::decode(&hex_str).map_err(|_| CryptoError::Wrap)?;
    bytes.try_into().map_err(|_| CryptoError::Wrap)
}

fn object_aad(generation: u32, id: &ObjectId) -> Vec<u8> {
    let mut aad = Vec::with_capacity(4 + 4 + 32);
    aad.extend_from_slice(b"obj1");
    aad.extend_from_slice(&generation.to_le_bytes());
    aad.extend_from_slice(id.as_bytes());
    aad
}

fn random_nonce() -> [u8; 24] {
    let mut nonce = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    nonce
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> CryptoError + '_ {
    move |source| CryptoError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), CryptoError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(io_err(path))?;
    file.write_all(bytes).map_err(io_err(path))?;
    file.sync_all().map_err(io_err(path))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_round_trip_and_reject_wrong_key() {
        let alice = BoxKeyPair::from_secret(random_bytes());
        let bob = BoxKeyPair::from_secret(random_bytes());
        let space = random_bytes();
        let wrapped = wrap_key(&bob.public_bytes(), &space).unwrap();
        assert_eq!(bob.unwrap(&wrapped).unwrap(), space);
        assert!(alice.unwrap(&wrapped).is_err());
    }

    #[test]
    fn seal_round_trip_binds_object_id() {
        let key = random_bytes();
        let id = ObjectId::of(b"hello");
        let sealed = seal_object(&key, 3, &id, b"hello").unwrap();
        assert!(!sealed.windows(5).any(|w| w == b"hello"));
        assert_eq!(sealed_generation(&sealed), Some(3));
        assert_eq!(open_object(&key, &id, &sealed).unwrap(), b"hello");
        let other = ObjectId::of(b"other");
        assert!(open_object(&key, &other, &sealed).is_err());
        let mut wrong = key;
        wrong[0] ^= 1;
        assert!(open_object(&wrong, &id, &sealed).is_err());
    }

    #[test]
    fn recovery_round_trip() {
        let secret = random_bytes();
        let space = random_bytes();
        let sealed = seal_recovery(&secret, &space).unwrap();
        assert_eq!(open_recovery(&secret, &sealed).unwrap(), space);
        let mut bad = secret;
        bad[1] ^= 0xff;
        assert!(open_recovery(&bad, &sealed).is_err());
        let text = format_recovery(&secret);
        assert_eq!(parse_recovery(&text).unwrap(), secret);
    }
}
