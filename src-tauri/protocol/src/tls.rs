//! QUIC transport setup.
//!
//! The controller presents a self-signed certificate; the agent does not verify
//! it at the TLS layer. That is deliberate and safe for this design: agents run
//! over the tailnet (already authenticated + encrypted end-to-end), and the
//! *application* layer authenticates the agent to the controller via Ed25519.
//! The TLS layer's job here is just to encrypt the QUIC stream.

use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::ALPN;

/// Install the ring crypto provider as the process default (idempotent).
pub fn install_crypto_provider() {
	let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Generate a self-signed certificate. Returns `(cert_der, pkcs8_key_der)`.
pub fn self_signed() -> (Vec<u8>, Vec<u8>) {
	let key = rcgen::generate_simple_self_signed(vec!["tether.local".to_string()]).expect("self-signed cert");
	(key.cert.der().to_vec(), key.key_pair.serialize_der())
}

fn transport() -> quinn::TransportConfig {
	let mut t = quinn::TransportConfig::default();
	// Keep these short so a dead/restarted controller is noticed quickly and
	// agents come back "online" within seconds rather than half a minute.
	t.max_idle_timeout(Some(Duration::from_secs(12).try_into().expect("idle timeout")));
	t.keep_alive_interval(Some(Duration::from_secs(4)));
	t
}

/// Build the controller's QUIC server config from a self-signed cert/key pair.
pub fn server_config(cert_der: Vec<u8>, key_der: Vec<u8>) -> quinn::ServerConfig {
	install_crypto_provider();
	let certs = vec![rustls::pki_types::CertificateDer::from(cert_der)];
	let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));

	let mut rustls_cfg = rustls::ServerConfig::builder()
		.with_no_client_auth()
		.with_single_cert(certs, key)
		.expect("valid server cert/key");
	rustls_cfg.alpn_protocols = vec![ALPN.to_vec()];

	let qsc = QuicServerConfig::try_from(rustls_cfg).expect("quic server config");
	let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(qsc));
	cfg.transport_config(Arc::new(transport()));
	cfg
}

/// Build the agent's QUIC client config (encrypt-only; identity is checked at
/// the application layer).
pub fn client_config() -> quinn::ClientConfig {
	install_crypto_provider();
	let mut rustls_cfg = rustls::ClientConfig::builder()
		.dangerous()
		.with_custom_certificate_verifier(Arc::new(danger::NoVerify))
		.with_no_client_auth();
	rustls_cfg.alpn_protocols = vec![ALPN.to_vec()];

	let qcc = QuicClientConfig::try_from(rustls_cfg).expect("quic client config");
	let mut cfg = quinn::ClientConfig::new(Arc::new(qcc));
	cfg.transport_config(Arc::new(transport()));
	cfg
}

mod danger {
	use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
	use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
	use rustls::{DigitallySignedStruct, Error, SignatureScheme};

	/// Accepts any certificate. See the module docs for why this is acceptable
	/// in Tether's threat model.
	#[derive(Debug)]
	pub struct NoVerify;

	impl ServerCertVerifier for NoVerify {
		fn verify_server_cert(
			&self,
			_end_entity: &CertificateDer<'_>,
			_intermediates: &[CertificateDer<'_>],
			_server_name: &ServerName<'_>,
			_ocsp_response: &[u8],
			_now: UnixTime,
		) -> Result<ServerCertVerified, Error> {
			Ok(ServerCertVerified::assertion())
		}

		fn verify_tls12_signature(
			&self,
			_message: &[u8],
			_cert: &CertificateDer<'_>,
			_dss: &DigitallySignedStruct,
		) -> Result<HandshakeSignatureValid, Error> {
			Ok(HandshakeSignatureValid::assertion())
		}

		fn verify_tls13_signature(
			&self,
			_message: &[u8],
			_cert: &CertificateDer<'_>,
			_dss: &DigitallySignedStruct,
		) -> Result<HandshakeSignatureValid, Error> {
			Ok(HandshakeSignatureValid::assertion())
		}

		fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
			vec![
				SignatureScheme::ED25519,
				SignatureScheme::ECDSA_NISTP256_SHA256,
				SignatureScheme::ECDSA_NISTP384_SHA384,
				SignatureScheme::RSA_PKCS1_SHA256,
				SignatureScheme::RSA_PSS_SHA256,
			]
		}
	}
}
