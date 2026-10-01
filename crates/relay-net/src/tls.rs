use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use relay_core::DeviceId;
use relay_crypto::{DeviceIdentity, device_id_from_certificate};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    DigitallySignedStruct, DistinguishedName, Error as TlsError, SignatureScheme,
    crypto::{self, verify_tls13_signature},
};

use crate::PeerConfig;
use crate::error::NetError;

/// SNI / certificate SAN. Identity is the pinned device key, not this name.
pub(crate) const SERVER_NAME: &str = "relay-device";

pub(crate) fn install_ring_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub(crate) fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(30)).expect("30s is a valid idle timeout"),
    ));
    transport.keep_alive_interval(Some(Duration::from_secs(10)));
    // The endpoint receive buffer is 1400 bytes. A relay DATA header is 21
    // bytes, so path MTU stays inside what that buffer can hold after wrapping.
    let mut discovery = quinn::MtuDiscoveryConfig::default();
    discovery.upper_bound(crate::relay::RELAY_PATH_MTU);
    transport.mtu_discovery_config(Some(discovery));
    Arc::new(transport)
}

#[derive(Clone)]
pub(crate) struct TlsMaterials {
    cert: CertificateDer<'static>,
    key_der: Vec<u8>,
}

impl TlsMaterials {
    pub(crate) fn from_identity(identity: &DeviceIdentity) -> Result<Self, NetError> {
        Ok(Self {
            cert: CertificateDer::from(identity.certificate_der()?),
            key_der: identity.private_key_der(),
        })
    }

    fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()))
    }

    fn cert_chain(&self) -> Vec<CertificateDer<'static>> {
        vec![self.cert.clone()]
    }
}

fn ring_sig_algs() -> &'static crypto::WebPkiSupportedAlgorithms {
    static ALGS: std::sync::LazyLock<crypto::WebPkiSupportedAlgorithms> =
        std::sync::LazyLock::new(|| {
            rustls::crypto::ring::default_provider().signature_verification_algorithms
        });
    &ALGS
}

fn verify_tls13(
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, TlsError> {
    verify_tls13_signature(message, cert, dss, ring_sig_algs())
}

fn reject_tls12() -> Result<HandshakeSignatureValid, TlsError> {
    Err(TlsError::General("TLS 1.2 is not supported".into()))
}

fn supported_schemes() -> Vec<SignatureScheme> {
    vec![SignatureScheme::ED25519]
}

fn device_id_or_bad_encoding(der: &[u8], side: &'static str) -> Result<DeviceId, TlsError> {
    device_id_from_certificate(der).map_err(|e| {
        tracing::warn!(error = %e, side, "peer certificate is not a relay device cert");
        TlsError::InvalidCertificate(rustls::CertificateError::BadEncoding)
    })
}

fn reject_untrusted(id: DeviceId, reason: &str) -> TlsError {
    tracing::warn!(peer = %id, "{reason}");
    TlsError::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure)
}

/// Accept any well-formed Relay device certificate. Trust is enforced after
/// the handshake, once ALPN is visible (`relay/1` vs `relay-pair/1`).
#[derive(Debug)]
struct AnyRelayClientVerifier;

impl ClientCertVerifier for AnyRelayClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        device_id_or_bad_encoding(end_entity.as_ref(), "client")?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        reject_tls12()
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        supported_schemes()
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }
}

/// Server-cert verifier for a specific dial: trusted *and* the expected peer.
#[derive(Debug)]
struct PinnedServerVerifier {
    expected: DeviceId,
    trusted: Arc<RwLock<HashMap<DeviceId, PeerConfig>>>,
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let id = device_id_or_bad_encoding(end_entity.as_ref(), "server")?;
        if id != self.expected {
            return Err(reject_untrusted(
                id,
                "server certificate is not the peer we dialed",
            ));
        }
        let trusted = self.trusted.read().unwrap_or_else(|e| e.into_inner());
        if !trusted.contains_key(&id) {
            return Err(reject_untrusted(
                id,
                "server certificate is not in the trusted set",
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        reject_tls12()
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        supported_schemes()
    }
}

/// Pairing dialer: accept any well-formed Relay device certificate.
#[derive(Debug)]
struct AnyRelayServerVerifier;

impl ServerCertVerifier for AnyRelayServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        device_id_or_bad_encoding(end_entity.as_ref(), "server")?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        reject_tls12()
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        supported_schemes()
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub(crate) fn make_server_config(
    materials: &TlsMaterials,
) -> Result<quinn::ServerConfig, NetError> {
    let verifier = Arc::new(AnyRelayClientVerifier);
    let mut rustls_cfg = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| NetError::Tls(e.to_string()))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(materials.cert_chain(), materials.key())
        .map_err(|e| NetError::Tls(e.to_string()))?;
    rustls_cfg.alpn_protocols = vec![relay_proto::ALPN.to_vec(), relay_proto::PAIR_ALPN.to_vec()];

    let mut server = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(rustls_cfg)
            .map_err(|e| NetError::Tls(e.to_string()))?,
    ));
    server.transport_config(transport_config());
    Ok(server)
}

pub(crate) fn make_client_config(
    materials: &TlsMaterials,
    expected: DeviceId,
    trusted: &Arc<RwLock<HashMap<DeviceId, PeerConfig>>>,
) -> Result<quinn::ClientConfig, NetError> {
    let verifier = Arc::new(PinnedServerVerifier {
        expected,
        trusted: trusted.clone(),
    });
    let mut rustls_cfg = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| NetError::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(materials.cert_chain(), materials.key())
        .map_err(|e| NetError::Tls(e.to_string()))?;
    rustls_cfg.alpn_protocols = vec![relay_proto::ALPN.to_vec()];

    let mut client = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(rustls_cfg)
            .map_err(|e| NetError::Tls(e.to_string()))?,
    ));
    client.transport_config(transport_config());
    Ok(client)
}

pub(crate) fn make_pairing_client_config(
    materials: &TlsMaterials,
) -> Result<quinn::ClientConfig, NetError> {
    let verifier = Arc::new(AnyRelayServerVerifier);
    let mut rustls_cfg = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| NetError::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(materials.cert_chain(), materials.key())
        .map_err(|e| NetError::Tls(e.to_string()))?;
    rustls_cfg.alpn_protocols = vec![relay_proto::PAIR_ALPN.to_vec()];

    let mut client = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(rustls_cfg)
            .map_err(|e| NetError::Tls(e.to_string()))?,
    ));
    client.transport_config(transport_config());
    Ok(client)
}
