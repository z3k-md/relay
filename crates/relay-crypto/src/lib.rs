//! Device identity and space-key cryptography.
//!
//! Each device owns one Ed25519 key pair. Its [`DeviceId`] *is* the raw
//! 32-byte public key, so a peer's identity is checked by comparing the key in
//! its TLS certificate against the pinned id; no certificate authority is
//! involved. The certificate itself is regenerated from the key on every start
//! and carries no meaning beyond holding the key.
//!
//! Space keys are separate: an X25519 box key wraps a random 256-bit key per
//! space, and mailbox objects are sealed with that key (see `secret`).

mod secret;

pub use secret::{
    BoxKeyPair, format_recovery, open_object, open_recovery, parse_recovery, random_bytes,
    seal_object, seal_recovery, sealed_generation, verify_device, wrap_key,
};

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rcgen::{CertificateParams, KeyPair, PKCS_ED25519};
use relay_core::DeviceId;

/// File under the identity directory holding the PKCS#8 PEM private key.
pub const KEY_FILE: &str = "device.key";

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("device key at {path} is invalid: {message}")]
    InvalidKey { path: PathBuf, message: String },
    #[error("device key already exists at {0}")]
    KeyExists(PathBuf),
    #[error("certificate generation failed: {0}")]
    Certificate(String),
    #[error("peer certificate is invalid: {0}")]
    PeerCertificate(String),
    #[error("sealed bytes are invalid")]
    Seal,
    #[error("key wrap is invalid")]
    Wrap,
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> CryptoError + '_ {
    move |source| CryptoError::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub struct DeviceIdentity {
    key: KeyPair,
    device_id: DeviceId,
}

impl std::fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

impl DeviceIdentity {
    /// Generate a new key and write it to `dir/device.key`. Refuses to
    /// overwrite an existing key: losing it changes this device's id.
    pub fn generate(dir: &Path) -> Result<DeviceIdentity, CryptoError> {
        let path = dir.join(KEY_FILE);
        if path.exists() {
            return Err(CryptoError::KeyExists(path));
        }
        fs::create_dir_all(dir).map_err(io_err(dir))?;
        let key = KeyPair::generate_for(&PKCS_ED25519)
            .map_err(|e| CryptoError::Certificate(e.to_string()))?;
        write_private(&path, key.serialize_pem().as_bytes())?;
        Self::from_key(key, &path)
    }

    pub fn load(dir: &Path) -> Result<DeviceIdentity, CryptoError> {
        let path = dir.join(KEY_FILE);
        let pem = fs::read_to_string(&path).map_err(io_err(&path))?;
        let key = KeyPair::from_pem(&pem).map_err(|e| CryptoError::InvalidKey {
            path: path.clone(),
            message: e.to_string(),
        })?;
        Self::from_key(key, &path)
    }

    pub fn exists(dir: &Path) -> bool {
        dir.join(KEY_FILE).is_file()
    }

    fn from_key(key: KeyPair, path: &Path) -> Result<DeviceIdentity, CryptoError> {
        if !key.is_compatible(&PKCS_ED25519) {
            return Err(CryptoError::InvalidKey {
                path: path.to_path_buf(),
                message: "not an Ed25519 key".into(),
            });
        }
        let raw: [u8; 32] =
            key.public_key_raw()
                .try_into()
                .map_err(|_| CryptoError::InvalidKey {
                    path: path.to_path_buf(),
                    message: "public key is not 32 bytes".into(),
                })?;
        Ok(DeviceIdentity {
            key,
            device_id: DeviceId::from_bytes(raw),
        })
    }

    pub fn device_id(&self) -> DeviceId {
        self.device_id
    }

    /// Fresh self-signed certificate (DER) for this key.
    pub fn certificate_der(&self) -> Result<Vec<u8>, CryptoError> {
        let params = CertificateParams::new(vec!["relay-device".to_owned()])
            .map_err(|e| CryptoError::Certificate(e.to_string()))?;
        let cert = params
            .self_signed(&self.key)
            .map_err(|e| CryptoError::Certificate(e.to_string()))?;
        Ok(cert.der().to_vec())
    }

    /// Ed25519 signature over `message` (the device identity key).
    pub fn sign(&self, message: &[u8]) -> Result<[u8; 64], CryptoError> {
        secret::sign_pem(&self.key.serialize_pem(), message)
    }

    /// PKCS#8 DER private key, for handing to rustls.
    pub fn private_key_der(&self) -> Vec<u8> {
        self.key.serialize_der()
    }
}

/// The device id a certificate claims: its Ed25519 public key. Callers must
/// still verify the TLS handshake signature (rustls does) and compare the
/// result against a pinned id.
pub fn device_id_from_certificate(der: &[u8]) -> Result<DeviceId, CryptoError> {
    use x509_parser::oid_registry::OID_SIG_ED25519;
    use x509_parser::prelude::{FromDer, X509Certificate};

    let (rest, cert) =
        X509Certificate::from_der(der).map_err(|e| CryptoError::PeerCertificate(e.to_string()))?;
    if !rest.is_empty() {
        return Err(CryptoError::PeerCertificate(
            "trailing bytes after certificate".into(),
        ));
    }
    let spki = cert.public_key();
    if spki.algorithm.algorithm != OID_SIG_ED25519 {
        return Err(CryptoError::PeerCertificate(
            "certificate key is not Ed25519".into(),
        ));
    }
    let raw: [u8; 32] = spki
        .subject_public_key
        .data
        .as_ref()
        .try_into()
        .map_err(|_| CryptoError::PeerCertificate("Ed25519 key is not 32 bytes".into()))?;
    Ok(DeviceId::from_bytes(raw))
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
    fn generated_identity_round_trips_and_matches_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let generated = DeviceIdentity::generate(dir.path()).unwrap();
        let loaded = DeviceIdentity::load(dir.path()).unwrap();
        assert_eq!(generated.device_id(), loaded.device_id());
        let cert = loaded.certificate_der().unwrap();
        assert_eq!(
            device_id_from_certificate(&cert).unwrap(),
            generated.device_id()
        );
    }

    #[test]
    fn generate_never_overwrites_an_existing_key() {
        let dir = tempfile::tempdir().unwrap();
        DeviceIdentity::generate(dir.path()).unwrap();
        assert!(matches!(
            DeviceIdentity::generate(dir.path()),
            Err(CryptoError::KeyExists(_))
        ));
    }

    #[test]
    fn garbage_certificates_are_rejected() {
        assert!(device_id_from_certificate(b"not a certificate").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        DeviceIdentity::generate(dir.path()).unwrap();
        let mode = fs::metadata(dir.path().join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
