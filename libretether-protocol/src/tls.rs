//! QUIC transport setup.
//!
//! The controller presents a self-signed certificate; peers do not verify it at
//! the TLS layer (`NoVerify`). TLS here only encrypts the QUIC stream — identity
//! is established at the *application* layer, where authentication is now
//! **mutual** and not dependent on the network being trusted:
//!
//! - the agent proves its identity to the controller (signs the controller's
//!   challenge nonce; matched against the key pinned at enrollment), and
//! - the controller proves its identity to the agent (signs the agent's nonce
//!   with the controller key the agent pinned via `--controller-key`).
//!
//! So even on an untrusted path (Direct mode over a port-forward, or through the
//! relay) an attacker that cannot present the expected certificate-independent
//! Ed25519 signatures is rejected. See `Challenge`/`Hello`/`HelloAck`.

use std::net::SocketAddr;
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
	let key = rcgen::generate_simple_self_signed(vec!["libretether.local".to_string()]).expect("self-signed cert");
	(key.cert.der().to_vec(), key.key_pair.serialize_der())
}

fn transport() -> quinn::TransportConfig {
	let mut t = quinn::TransportConfig::default();
	// Keep these short so a dead/restarted controller is noticed quickly and
	// agents come back "online" within seconds rather than half a minute.
	t.max_idle_timeout(Some(Duration::from_secs(12).try_into().expect("idle timeout")));
	t.keep_alive_interval(Some(Duration::from_secs(4)));
	// Explicitly bound how many bidirectional streams a peer may have open at once.
	// This caps the per-connection fan-out (each stream spawns a task on the agent
	// and the relay), so a buggy or hostile peer can't grow it without limit — while
	// leaving generous headroom for the relay, whose single controller connection
	// multiplexes control/session/tunnel streams for every agent at once.
	t.max_concurrent_bidi_streams(1024u32.into());
	t.max_concurrent_uni_streams(0u32.into());
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

/// Build a QUIC **server** [`Endpoint`](quinn::Endpoint) bound to `addr`.
///
/// When `addr` is IPv6 the socket is made dual-stack — `IPV6_V6ONLY` is cleared —
/// so a `[::]` listener accepts IPv4-mapped clients as well as native IPv6 ones.
/// That option defaults *on* under Windows and some BSDs, where a plain `[::]`
/// bind would silently refuse every IPv4 agent; clearing it makes one socket serve
/// both families uniformly across platforms. quinn's `Endpoint::server` binds a
/// blocking `UdpSocket` with default options and gives no hook to set this, so we
/// bind the socket ourselves and hand it to `Endpoint::new`.
///
/// The controller listens on `[::]` (so agents reach it over either family) and the
/// relay's default `[::]` listen gets the same behavior; a specific v4/v6 bind is
/// honored as-is.
pub fn server_endpoint(cert_der: Vec<u8>, key_der: Vec<u8>, addr: SocketAddr) -> std::io::Result<quinn::Endpoint> {
	let socket = bind_server_socket(addr)?;
	quinn::Endpoint::new(
		quinn::EndpointConfig::default(),
		Some(server_config(cert_der, key_der)),
		socket,
		Arc::new(quinn::TokioRuntime),
	)
}

/// Bind the UDP socket for [`server_endpoint`], clearing `IPV6_V6ONLY` on IPv6
/// binds so `[::]` is dual-stack everywhere (see that function). Split out so the
/// socket-option handling lives in one place. quinn sets the socket non-blocking
/// when it wraps it, so we leave that to the runtime.
fn bind_server_socket(addr: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
	use socket2::{Domain, Protocol, Socket, Type};
	let domain = if addr.is_ipv6() { Domain::IPV6 } else { Domain::IPV4 };
	let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
	if addr.is_ipv6() {
		// Accept IPv4-mapped clients on a `[::]` bind. Best-effort: a platform that
		// refuses the toggle still binds (IPv6-only); a specific v6 bind is unaffected.
		let _ = socket.set_only_v6(false);
	}
	socket.bind(&addr.into())?;
	Ok(socket.into())
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

/// Build a QUIC client [`Endpoint`](quinn::Endpoint) for dialing `target`, bound
/// to the target's address family.
///
/// quinn rejects dialing an IPv6 peer from an IPv4 socket (and vice versa) with
/// "invalid remote address", so an IPv6 peer needs a `[::]` client and an IPv4
/// peer a `0.0.0.0` one. Shared by the agent and the controller.
pub fn client_endpoint(target: SocketAddr) -> std::io::Result<quinn::Endpoint> {
	let bind: SocketAddr = if target.is_ipv6() {
		(std::net::Ipv6Addr::UNSPECIFIED, 0).into()
	} else {
		(std::net::Ipv4Addr::UNSPECIFIED, 0).into()
	};
	let mut endpoint = quinn::Endpoint::client(bind)?;
	endpoint.set_default_client_config(client_config());
	Ok(endpoint)
}

/// Build a **dual-role** QUIC [`Endpoint`](quinn::Endpoint) — one that can both dial
/// out (client) *and* accept incoming connections (server) — on a single ephemeral
/// UDP socket whose family matches `target`.
///
/// This is the endpoint the controller uses in relay mode for **peer-to-peer NAT
/// traversal**: it dials the relay *and* the punching agent as a client, and accepts
/// the agent's direct connection as a server, all on the *same* socket — which is what
/// makes hole-punching work, since a NAT mapping is keyed to the source port. The
/// server side presents the controller's own certificate (identity is still checked
/// at the application layer via the Ed25519 handshake, exactly as in Direct mode).
pub fn dual_endpoint(cert_der: Vec<u8>, key_der: Vec<u8>, target: SocketAddr) -> std::io::Result<quinn::Endpoint> {
	let bind: SocketAddr = if target.is_ipv6() {
		(std::net::Ipv6Addr::UNSPECIFIED, 0).into()
	} else {
		(std::net::Ipv4Addr::UNSPECIFIED, 0).into()
	};
	let socket = std::net::UdpSocket::bind(bind)?;
	let mut endpoint = quinn::Endpoint::new(
		quinn::EndpointConfig::default(),
		Some(server_config(cert_der, key_der)),
		socket,
		Arc::new(quinn::TokioRuntime),
	)?;
	endpoint.set_default_client_config(client_config());
	Ok(endpoint)
}

/// Resolve `addr` (an `ip:port` literal or a `host:port` name) to a single socket
/// address, preferring the literal parse and falling back to DNS.
pub async fn resolve(addr: &str) -> std::io::Result<SocketAddr> {
	if let Ok(sa) = addr.parse::<SocketAddr>() {
		return Ok(sa);
	}
	tokio::net::lookup_host(addr)
		.await?
		.next()
		.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, format!("no address resolved for {addr}")))
}

mod danger {
	use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
	use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
	use rustls::{DigitallySignedStruct, Error, SignatureScheme};

	/// Accepts any certificate. See the module docs for why this is acceptable
	/// in LibreTether's threat model.
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

#[cfg(test)]
mod tests {
	use super::*;
	use std::net::{Ipv4Addr, Ipv6Addr};

	/// A `[::]` server endpoint must accept a client dialing it over IPv4 (as an
	/// IPv4-mapped peer) — this is the dual-stack guarantee `server_endpoint` adds
	/// over a plain `Endpoint::server([::])`, which is IPv6-only under Windows/BSD.
	/// This is exactly the path a Direct-mode agent takes when the controller now
	/// listens on `[::]` but the agent reaches it over IPv4.
	#[tokio::test]
	async fn server_endpoint_on_v6_wildcard_accepts_an_ipv4_client() {
		install_crypto_provider();
		let (cert, key) = self_signed();
		let server = server_endpoint(cert, key, (Ipv6Addr::UNSPECIFIED, 0).into()).expect("bind dual-stack server");
		let port = server.local_addr().unwrap().port();

		let accept = tokio::spawn(async move {
			let incoming = server.accept().await.expect("incoming connection");
			incoming.await.expect("handshake completes");
		});

		// Dial over IPv4 loopback — the dual-stack listener sees it as `::ffff:127.0.0.1`.
		let target: SocketAddr = (Ipv4Addr::LOCALHOST, port).into();
		let client = client_endpoint(target).expect("client endpoint");
		client
			.connect(target, "libretether.local")
			.expect("start connect")
			.await
			.expect("connect over IPv4 to the [::] listener");
		accept.await.unwrap();
	}

	/// The mechanism peer-to-peer hole-punching depends on: a single dual-role
	/// endpoint must be able to **accept** an incoming connection *and* **dial out** —
	/// both on its one shared socket. Without this, the controller couldn't reuse the
	/// socket the relay observed, and the NAT mapping wouldn't line up.
	#[tokio::test]
	async fn dual_endpoint_can_both_accept_and_dial_on_one_socket() {
		install_crypto_provider();
		let (cert_a, key_a) = self_signed();
		let (cert_b, key_b) = self_signed();
		let probe: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
		let a = dual_endpoint(cert_a, key_a, probe).expect("dual endpoint A");
		let b = dual_endpoint(cert_b, key_b, probe).expect("dual endpoint B");
		// `local_addr` reports the wildcard bind (`0.0.0.0:port`); dial the loopback IP
		// with each endpoint's actual port.
		let a_addr: SocketAddr = (Ipv4Addr::LOCALHOST, a.local_addr().unwrap().port()).into();
		let b_addr: SocketAddr = (Ipv4Addr::LOCALHOST, b.local_addr().unwrap().port()).into();

		// B (client role) dials A (server role); A accepts on the same socket it will
		// also dial from below.
		let a_accept = {
			let a = a.clone();
			tokio::spawn(async move { a.accept().await.unwrap().accept().unwrap().await.unwrap() })
		};
		let b_to_a = b.connect(a_addr, "libretether.local").unwrap().await.unwrap();
		let a_from_b = a_accept.await.unwrap();

		// Now A (client role) dials B (server role) — proving the *same* endpoint that
		// just accepted can also initiate a connection.
		let b_accept = {
			let b = b.clone();
			tokio::spawn(async move { b.accept().await.unwrap().accept().unwrap().await.unwrap() })
		};
		let a_to_b = a.connect(b_addr, "libretether.local").unwrap().await.unwrap();
		let b_from_a = b_accept.await.unwrap();

		// Both directions carry data, confirming two independent live connections on the
		// two shared sockets.
		let (mut s, _r) = a_from_b.open_bi().await.unwrap();
		s.write_all(b"ping").await.unwrap();
		s.finish().unwrap();
		let (_s2, mut r2) = b_to_a.accept_bi().await.unwrap();
		assert_eq!(r2.read_to_end(16).await.unwrap(), b"ping");

		// Keep the second pair alive to the end so the connect isn't dropped early.
		let _ = (a_to_b, b_from_a);
	}
}
