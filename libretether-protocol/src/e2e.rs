//! End-to-end encryption for the controller↔agent link, so a relay that only
//! forwards bytes never sees plaintext.
//!
//! In relay mode the relay terminates QUIC/TLS on both hops and pipes the
//! decrypted stream between the two ends, so QUIC's transport encryption alone
//! leaves the *payload* readable by the relay host. This module wraps every
//! post-handshake stream in an application-layer AEAD whose key is agreed
//! end-to-end and bound to the Ed25519 identities already pinned at enrollment —
//! the same guarantee WireGuard gives Tailscale's DERP path. It is always on (not
//! just over a relay): a single record layer with no "is this encrypted?" branch
//! fails closed, and the cost (ChaCha20-Poly1305 over the session) is negligible.
//!
//! ## Key agreement
//!
//! The agreement is a signed ephemeral ECDHE (a station-to-station AKE) folded
//! into the existing mutual handshake, *not* a fresh protocol:
//!
//! 1. Each side generates an ephemeral X25519 keypair and sends its public half in
//!    the handshake ([`crate::Challenge::controller_eph`], [`crate::Hello::agent_eph`]).
//! 2. Both sign the **same** [`handshake_transcript`] — which commits to *both*
//!    ephemeral keys and *both* nonces — with their long-term Ed25519 identity
//!    (the agent in `Hello.signature`, the controller in `HelloAck.controller_sig`),
//!    and each verifies the other's signature against the key it pinned at
//!    enrollment. Because the transcript pins the ephemeral keys, a
//!    man-in-the-middle relay cannot substitute its own without invalidating a
//!    signature it can't forge.
//! 3. The X25519 shared secret is run through HKDF (salted by the transcript) into
//!    a per-connection [`SessionKey`]. Ephemeral keys give forward secrecy; the
//!    relay, lacking either private key, can't derive it.
//!
//! ## Record layer
//!
//! Each non-handshake stream is opened as usual (its `StreamOpen` stays plaintext
//! so the agent can still tell a handshake stream apart), then the controller
//! sends a random 32-byte per-stream salt and both derive a fresh pair of
//! directional keys from the [`SessionKey`] — so every stream, and each direction
//! within it, has a unique key and can restart its nonce counter at zero with no
//! risk of reuse. Everything after the salt (including the capability token) is a
//! sequence of AEAD records via [`SecureSend`]/[`SecureRecv`], which implement
//! `AsyncRead`/`AsyncWrite` so the framing, video and tunnel code above them is
//! oblivious to the encryption.

use std::io;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

use crate::crypto::fill_random;

/// Domain separator over the signed transcript and the derived keys. Bumped with
/// the wire format (paired with the controller/agent shipping together).
const DOMAIN: &[u8] = b"libretether-e2e-v1";

/// Largest plaintext chunk in one AEAD record. A larger write is split across
/// records; the 16-byte tag per record is negligible overhead at this size.
const RECORD_MAX_PLAIN: usize = 16 * 1024;
/// Largest ciphertext (plaintext + Poly1305 tag) a record may carry. The reader
/// rejects a length prefix above this, so a desynced or hostile peer can't force
/// a huge allocation.
const MAX_CT: usize = RECORD_MAX_PLAIN + 16;
/// Per-stream salt length (bytes) exchanged before the record stream begins.
pub const STREAM_SALT_LEN: usize = 32;

// ------------------------------------------------------------------ key agreement

/// A single-use ephemeral X25519 keypair for one handshake. Seeded from the OS
/// RNG (`crypto::fill_random`) so the RNG failure policy stays in one place, and
/// consumed by [`Self::diffie_hellman`] so the private half can't outlive the
/// exchange (forward secrecy).
pub struct EphemeralKeypair {
	secret: x25519_dalek::StaticSecret,
	public: [u8; 32],
}

impl EphemeralKeypair {
	/// Generate a fresh ephemeral keypair.
	pub fn generate() -> Self {
		let mut seed = [0u8; 32];
		fill_random(&mut seed);
		let secret = x25519_dalek::StaticSecret::from(seed);
		let public = x25519_dalek::PublicKey::from(&secret).to_bytes();
		Self { secret, public }
	}

	/// The base64 public key to put on the wire.
	pub fn public_b64(&self) -> String {
		B64.encode(self.public)
	}

	/// The raw 32-byte public key, for building the signed transcript.
	pub fn public_bytes(&self) -> [u8; 32] {
		self.public
	}

	/// Complete the exchange with the peer's public key, consuming our secret.
	/// Returns `None` if the DH output is all-zero (a low-order peer point) — a
	/// degenerate shared secret is rejected rather than used, so the channel fails
	/// closed even though the peer's ephemeral key is already signature-authenticated.
	pub fn diffie_hellman(self, peer_public: &[u8; 32]) -> Option<[u8; 32]> {
		let shared = self
			.secret
			.diffie_hellman(&x25519_dalek::PublicKey::from(*peer_public))
			.to_bytes();
		(shared != [0u8; 32]).then_some(shared)
	}
}

/// Decode a base64 X25519 public key to its 32 raw bytes, or `None` if it isn't
/// valid base64 of exactly 32 bytes.
pub fn decode_eph(public_b64: &str) -> Option<[u8; 32]> {
	B64.decode(public_b64.trim()).ok()?.try_into().ok()
}

/// The message both ends sign (and verify against the peer's pinned Ed25519 key)
/// to authenticate the exchange. It commits to both ephemeral public keys and both
/// nonces, so a relay that swaps in its own ephemeral key invalidates a signature
/// it cannot forge — which is what stops it machine-in-the-middling the channel.
pub fn handshake_transcript(
	controller_eph: &[u8; 32],
	agent_eph: &[u8; 32],
	nonce: &str,
	agent_nonce: &str,
) -> Vec<u8> {
	let mut t = Vec::with_capacity(DOMAIN.len() + 64 + nonce.len() + agent_nonce.len() + 16);
	t.extend_from_slice(DOMAIN);
	t.extend_from_slice(controller_eph);
	t.extend_from_slice(agent_eph);
	// Length-prefix the variable-length nonces so the concatenation is unambiguous.
	t.extend_from_slice(&(nonce.len() as u32).to_be_bytes());
	t.extend_from_slice(nonce.as_bytes());
	t.extend_from_slice(&(agent_nonce.len() as u32).to_be_bytes());
	t.extend_from_slice(agent_nonce.as_bytes());
	t
}

/// The per-connection key agreed by the handshake. All record keys are HKDF-derived
/// from it, so it never encrypts anything directly.
#[derive(Clone)]
pub struct SessionKey([u8; 32]);

impl SessionKey {
	/// Derive the session key from the X25519 shared secret, salted by the signed
	/// transcript so it's bound to the exact authenticated exchange.
	pub fn derive(shared: &[u8; 32], transcript: &[u8]) -> Self {
		let mut key = [0u8; 32];
		Hkdf::<Sha256>::new(Some(transcript), shared)
			.expand(b"libretether-e2e session", &mut key)
			.expect("hkdf session key within length");
		Self(key)
	}

	/// Derive the two directional record keys for one stream from its random salt:
	/// `(controller→agent, agent→controller)`. A fresh salt per stream means each
	/// stream's keys are unique, so every direction can start its nonce counter at
	/// zero without ever reusing a (key, nonce) pair.
	fn stream_keys(&self, salt: &[u8; STREAM_SALT_LEN]) -> ([u8; 32], [u8; 32]) {
		let hk = Hkdf::<Sha256>::new(Some(salt), &self.0);
		let mut c2a = [0u8; 32];
		let mut a2c = [0u8; 32];
		hk.expand(b"c2a", &mut c2a).expect("hkdf c2a within length");
		hk.expand(b"a2c", &mut a2c).expect("hkdf a2c within length");
		(c2a, a2c)
	}
}

// ------------------------------------------------------------------ record layer

/// The 12-byte AEAD nonce for record `counter`: the counter little-endian in the
/// low 8 bytes, the rest zero. Unique within a stream direction because the counter
/// is monotonic and the key is per-stream.
fn record_nonce(counter: u64) -> Nonce {
	let mut n = [0u8; 12];
	n[..8].copy_from_slice(&counter.to_le_bytes());
	Nonce::from(n)
}

/// The encrypting half of a secured stream: writes each chunk of plaintext as one
/// length-prefixed AEAD record. Implements [`AsyncWrite`] so the framing/video/
/// tunnel writers above it need no changes.
pub struct SecureSend<W> {
	inner: W,
	cipher: ChaCha20Poly1305,
	counter: u64,
	/// The current record's ciphertext (length prefix included) being flushed to
	/// `inner`, and how much of it has gone out. Empty between records.
	out: Vec<u8>,
	out_pos: usize,
	/// Plaintext bytes the in-flight record represents, reported to the caller only
	/// once the record is fully flushed (so a returned `Ok(n)` always means `n`
	/// bytes are on the wire, never merely encrypted-but-stuck).
	pending_plain: usize,
}

impl<W: AsyncWrite + Unpin> SecureSend<W> {
	fn new(inner: W, key: [u8; 32]) -> Self {
		Self {
			inner,
			cipher: ChaCha20Poly1305::new(&Key::from(key)),
			counter: 0,
			out: Vec::new(),
			out_pos: 0,
			pending_plain: 0,
		}
	}

	/// Drive the pending record out to `inner`, preserving progress across a
	/// `Poll::Pending`. Resolves once `out` is fully written (and cleared).
	fn drive_out(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		while self.out_pos < self.out.len() {
			let n = ready!(Pin::new(&mut self.inner).poll_write(cx, &self.out[self.out_pos..]))?;
			if n == 0 {
				return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
			}
			self.out_pos += n;
		}
		self.out.clear();
		self.out_pos = 0;
		Poll::Ready(Ok(()))
	}
}

impl<W: AsyncWrite + Unpin> AsyncWrite for SecureSend<W> {
	fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
		let me = self.get_mut();
		// Finish flushing a record left over from a prior `Pending` before taking new
		// plaintext, so at most one record is ever in flight.
		if !me.out.is_empty() {
			ready!(me.drive_out(cx))?;
			let n = me.pending_plain;
			me.pending_plain = 0;
			return Poll::Ready(Ok(n));
		}
		if buf.is_empty() {
			return Poll::Ready(Ok(0));
		}
		let take = buf.len().min(RECORD_MAX_PLAIN);
		let ct = me
			.cipher
			.encrypt(&record_nonce(me.counter), &buf[..take])
			.map_err(|_| io::Error::other("e2e: encrypt failed"))?;
		me.counter += 1;
		me.out.reserve(4 + ct.len());
		me.out.extend_from_slice(&(ct.len() as u32).to_be_bytes());
		me.out.extend_from_slice(&ct);
		me.out_pos = 0;
		me.pending_plain = take;
		ready!(me.drive_out(cx))?;
		let n = me.pending_plain;
		me.pending_plain = 0;
		Poll::Ready(Ok(n))
	}

	fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		let me = self.get_mut();
		ready!(me.drive_out(cx))?;
		Pin::new(&mut me.inner).poll_flush(cx)
	}

	fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		let me = self.get_mut();
		ready!(me.drive_out(cx))?;
		Pin::new(&mut me.inner).poll_shutdown(cx)
	}
}

/// Which part of the next record the reader is currently accumulating.
enum ReadPhase {
	/// Reading the 4-byte big-endian ciphertext length prefix.
	Len,
	/// Reading `usize` ciphertext bytes, then decrypting them.
	Body(usize),
}

/// The decrypting half of a secured stream: reassembles length-prefixed AEAD
/// records, decrypts them (rejecting any tampering or truncation), and serves the
/// plaintext. Implements [`AsyncRead`] so the readers above it need no changes.
pub struct SecureRecv<R> {
	inner: R,
	cipher: ChaCha20Poly1305,
	counter: u64,
	phase: ReadPhase,
	/// Bytes read so far toward the current phase's target (length or body).
	acc: Vec<u8>,
	/// Decrypted plaintext of the last record, and how much has been handed out.
	plain: Vec<u8>,
	plain_pos: usize,
}

impl<R: AsyncRead + Unpin> SecureRecv<R> {
	fn new(inner: R, key: [u8; 32]) -> Self {
		Self {
			inner,
			cipher: ChaCha20Poly1305::new(&Key::from(key)),
			counter: 0,
			phase: ReadPhase::Len,
			acc: Vec::new(),
			plain: Vec::new(),
			plain_pos: 0,
		}
	}

	/// Read from `inner` until `acc` holds `need` bytes. `Ok(true)` = full; `Ok(false)`
	/// = clean EOF with nothing buffered (only valid between records). A short read
	/// mid-field is a truncation error — the stream fails closed rather than
	/// accepting a partial record.
	fn fill(&mut self, cx: &mut Context<'_>, need: usize) -> Poll<io::Result<bool>> {
		while self.acc.len() < need {
			let mut scratch = [0u8; 4096];
			let want = (need - self.acc.len()).min(scratch.len());
			let mut buf = ReadBuf::new(&mut scratch[..want]);
			ready!(Pin::new(&mut self.inner).poll_read(cx, &mut buf))?;
			let filled = buf.filled();
			if filled.is_empty() {
				return if self.acc.is_empty() {
					Poll::Ready(Ok(false))
				} else {
					Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()))
				};
			}
			self.acc.extend_from_slice(filled);
		}
		Poll::Ready(Ok(true))
	}
}

impl<R: AsyncRead + Unpin> AsyncRead for SecureRecv<R> {
	fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, out: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
		let me = self.get_mut();
		loop {
			// Serve any buffered plaintext first.
			if me.plain_pos < me.plain.len() {
				let n = out.remaining().min(me.plain.len() - me.plain_pos);
				out.put_slice(&me.plain[me.plain_pos..me.plain_pos + n]);
				me.plain_pos += n;
				return Poll::Ready(Ok(()));
			}
			match me.phase {
				ReadPhase::Len => {
					if !ready!(me.fill(cx, 4))? {
						// Clean EOF at a record boundary — end of stream.
						return Poll::Ready(Ok(()));
					}
					let ct_len = u32::from_be_bytes(me.acc[..4].try_into().unwrap()) as usize;
					me.acc.clear();
					// A record always carries at least the 16-byte tag; anything larger
					// than one record, or too small to hold a tag, is corruption/desync.
					if !(16..=MAX_CT).contains(&ct_len) {
						return Poll::Ready(Err(io::Error::new(
							io::ErrorKind::InvalidData,
							format!("e2e: bad record length {ct_len}"),
						)));
					}
					me.phase = ReadPhase::Body(ct_len);
				}
				ReadPhase::Body(ct_len) => {
					if !ready!(me.fill(cx, ct_len))? {
						// EOF where a body was promised by the length prefix.
						return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
					}
					let plain = me
						.cipher
						.decrypt(&record_nonce(me.counter), me.acc.as_slice())
						.map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "e2e: record authentication failed"))?;
					me.counter += 1;
					me.acc.clear();
					me.phase = ReadPhase::Len;
					me.plain = plain;
					me.plain_pos = 0;
					// Loop to serve the freshly decrypted plaintext.
				}
			}
		}
	}
}

// ------------------------------------------------------------------ stream setup

/// The controller's side of securing a freshly opened stream: pick a random
/// per-stream salt, send it in the clear (it isn't secret), derive the directional
/// keys, and wrap the halves. Called right after the plaintext `StreamOpen`, so the
/// capability token and everything after it is encrypted.
pub async fn open_secure_controller<W, R>(
	mut send: W,
	recv: R,
	key: &SessionKey,
) -> io::Result<(SecureSend<W>, SecureRecv<R>)>
where
	W: AsyncWrite + Unpin,
	R: AsyncRead + Unpin,
{
	let mut salt = [0u8; STREAM_SALT_LEN];
	fill_random(&mut salt);
	send.write_all(&salt).await?;
	send.flush().await?;
	let (c2a, a2c) = key.stream_keys(&salt);
	Ok((SecureSend::new(send, c2a), SecureRecv::new(recv, a2c)))
}

/// The agent's side: read the controller's per-stream salt, derive the same
/// directional keys, and wrap the halves.
pub async fn open_secure_agent<W, R>(
	send: W,
	mut recv: R,
	key: &SessionKey,
) -> io::Result<(SecureSend<W>, SecureRecv<R>)>
where
	W: AsyncWrite + Unpin,
	R: AsyncRead + Unpin,
{
	let mut salt = [0u8; STREAM_SALT_LEN];
	recv.read_exact(&mut salt).await?;
	let (c2a, a2c) = key.stream_keys(&salt);
	// The agent sends on a2c and receives on c2a — the mirror of the controller.
	Ok((SecureSend::new(send, a2c), SecureRecv::new(recv, c2a)))
}

/// Convenience aliases for the common case of wrapping raw QUIC streams.
pub type SecureQuicSend = SecureSend<quinn::SendStream>;
pub type SecureQuicRecv = SecureRecv<quinn::RecvStream>;

#[cfg(test)]
mod tests {
	use super::*;
	use crate::crypto::{verify_b64, Identity};

	/// Two ends that used matching ephemeral keys + transcript derive the same
	/// session key, and thus the same per-stream directional keys.
	#[test]
	fn key_agreement_converges_and_binds_the_transcript() {
		let ctrl = EphemeralKeypair::generate();
		let agent = EphemeralKeypair::generate();
		let cpub = decode_eph(&ctrl.public_b64()).unwrap();
		let apub = decode_eph(&agent.public_b64()).unwrap();
		let transcript = handshake_transcript(&cpub, &apub, "nonce-c", "nonce-a");

		let c_shared = ctrl.diffie_hellman(&apub).unwrap();
		let a_shared = agent.diffie_hellman(&cpub).unwrap();
		assert_eq!(c_shared, a_shared, "DH is symmetric");

		let c_key = SessionKey::derive(&c_shared, &transcript);
		let a_key = SessionKey::derive(&a_shared, &transcript);
		let salt = [7u8; STREAM_SALT_LEN];
		assert_eq!(c_key.stream_keys(&salt), a_key.stream_keys(&salt));

		// A different transcript (e.g. a swapped ephemeral key) yields a different key.
		let tampered = handshake_transcript(&[0u8; 32], &apub, "nonce-c", "nonce-a");
		assert_ne!(
			SessionKey::derive(&c_shared, &tampered).stream_keys(&salt),
			c_key.stream_keys(&salt)
		);
	}

	// Both ends sign the identical transcript with their long-term identity, and each
	// verifies the other's against the pinned key — the authentication that stops a
	// relay swapping ephemeral keys.
	#[test]
	fn transcript_signatures_verify_against_pinned_identities() {
		let ctrl_id = Identity::generate();
		let agent_id = Identity::generate();
		let ctrl = EphemeralKeypair::generate();
		let agent = EphemeralKeypair::generate();
		let cpub = decode_eph(&ctrl.public_b64()).unwrap();
		let apub = decode_eph(&agent.public_b64()).unwrap();
		let t = handshake_transcript(&cpub, &apub, "n-c", "n-a");

		let agent_sig = agent_id.sign_b64(&t);
		let ctrl_sig = ctrl_id.sign_b64(&t);
		assert!(verify_b64(&agent_id.public_b64(), &t, &agent_sig));
		assert!(verify_b64(&ctrl_id.public_b64(), &t, &ctrl_sig));
		// A transcript with a substituted ephemeral key won't verify the real signature.
		let forged = handshake_transcript(&[9u8; 32], &apub, "n-c", "n-a");
		assert!(!verify_b64(&ctrl_id.public_b64(), &forged, &ctrl_sig));
	}

	#[test]
	fn diffie_hellman_rejects_a_low_order_point() {
		// The all-zero point has small order; the DH output is all-zero and must be
		// rejected rather than used as a key.
		let eph = EphemeralKeypair::generate();
		assert!(eph.diffie_hellman(&[0u8; 32]).is_none());
	}

	/// A controller/agent pair of secured streams over an in-memory duplex, sharing
	/// a session key and salt. Returns `((c_send, c_recv), (a_send, a_recv))`.
	async fn secured_pair() -> (
		(SecureSend<impl AsyncWrite + Unpin>, SecureRecv<impl AsyncRead + Unpin>),
		(SecureSend<impl AsyncWrite + Unpin>, SecureRecv<impl AsyncRead + Unpin>),
	) {
		let key = SessionKey([42u8; 32]);
		let (c_io, a_io) = tokio::io::duplex(1 << 16);
		let (c_read, c_write) = tokio::io::split(c_io);
		let (a_read, a_write) = tokio::io::split(a_io);
		// Run both sides' salt exchange concurrently (the controller writes the salt,
		// the agent reads it).
		let key2 = key.clone();
		let ctrl = tokio::spawn(async move { open_secure_controller(c_write, c_read, &key2).await.unwrap() });
		let agent = open_secure_agent(a_write, a_read, &key).await.unwrap();
		(ctrl.await.unwrap(), agent)
	}

	#[tokio::test]
	async fn round_trips_bytes_in_both_directions() {
		let ((mut cs, mut cr), (mut as_, mut ar)) = secured_pair().await;
		// Controller → agent.
		cs.write_all(b"hello agent").await.unwrap();
		cs.flush().await.unwrap();
		let mut buf = [0u8; 11];
		ar.read_exact(&mut buf).await.unwrap();
		assert_eq!(&buf, b"hello agent");
		// Agent → controller.
		as_.write_all(b"hi controller").await.unwrap();
		as_.flush().await.unwrap();
		let mut buf = [0u8; 13];
		cr.read_exact(&mut buf).await.unwrap();
		assert_eq!(&buf, b"hi controller");
	}

	#[tokio::test]
	async fn round_trips_a_payload_larger_than_one_record() {
		// A 200 KiB payload spans many 16 KiB records; every byte must survive in order.
		let ((mut cs, _cr), (_as, mut ar)) = secured_pair().await;
		let big: Vec<u8> = (0..200_000).map(|i| (i * 31 + 7) as u8).collect();
		let expected = big.clone();
		let writer = tokio::spawn(async move {
			cs.write_all(&big).await.unwrap();
			cs.shutdown().await.unwrap();
		});
		let mut got = Vec::new();
		ar.read_to_end(&mut got).await.unwrap();
		writer.await.unwrap();
		assert_eq!(got, expected);
	}

	#[tokio::test]
	async fn clean_eof_after_shutdown_reads_as_end_of_stream() {
		let ((mut cs, _cr), (_as, mut ar)) = secured_pair().await;
		cs.write_all(b"tail").await.unwrap();
		cs.shutdown().await.unwrap();
		let mut got = Vec::new();
		ar.read_to_end(&mut got).await.unwrap();
		assert_eq!(got, b"tail");
	}

	#[tokio::test]
	async fn tampered_ciphertext_fails_the_read() {
		// Encrypt one record, flip a bit in its ciphertext, and confirm the decrypt
		// rejects it (the AEAD tag catches tampering) rather than yielding plaintext.
		let key = SessionKey([1u8; 32]);
		let salt = [2u8; STREAM_SALT_LEN];
		let (c2a, _a2c) = key.stream_keys(&salt);
		let mut buf = Vec::new();
		let mut sender = SecureSend::new(&mut buf, c2a);
		sender.write_all(b"secret payload").await.unwrap();
		sender.flush().await.unwrap();
		// Flip a byte inside the ciphertext (skip the 4-byte length prefix).
		buf[6] ^= 0x80;
		let mut reader = SecureRecv::new(&buf[..], c2a);
		let mut out = Vec::new();
		let err = reader.read_to_end(&mut out).await.unwrap_err();
		assert_eq!(err.kind(), io::ErrorKind::InvalidData);
	}

	#[tokio::test]
	async fn a_different_key_cannot_decrypt() {
		let salt = [3u8; STREAM_SALT_LEN];
		let (right, _) = SessionKey([4u8; 32]).stream_keys(&salt);
		let (wrong, _) = SessionKey([5u8; 32]).stream_keys(&salt);
		let mut buf = Vec::new();
		let mut sender = SecureSend::new(&mut buf, right);
		sender.write_all(b"for the right key only").await.unwrap();
		sender.flush().await.unwrap();
		let mut reader = SecureRecv::new(&buf[..], wrong);
		let mut out = Vec::new();
		assert!(reader.read_to_end(&mut out).await.is_err());
	}

	#[tokio::test]
	async fn truncated_record_is_rejected() {
		// A length prefix promising more ciphertext than arrives must fail closed, not
		// hang or yield a short read.
		let key = SessionKey([6u8; 32]);
		let salt = [7u8; STREAM_SALT_LEN];
		let (c2a, _) = key.stream_keys(&salt);
		let mut buf = Vec::new();
		let mut sender = SecureSend::new(&mut buf, c2a);
		sender.write_all(b"complete record").await.unwrap();
		sender.flush().await.unwrap();
		buf.truncate(buf.len() - 4); // drop the tail of the ciphertext
		let mut reader = SecureRecv::new(&buf[..], c2a);
		let mut out = Vec::new();
		let err = reader.read_to_end(&mut out).await.unwrap_err();
		assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
	}

	#[tokio::test]
	async fn byte_at_a_time_reader_reassembles_records() {
		// A reader that yields a single byte per poll exercises the partial-read
		// accumulator across the length prefix and body of multiple records.
		let key = SessionKey([8u8; 32]);
		let salt = [9u8; STREAM_SALT_LEN];
		let (c2a, _) = key.stream_keys(&salt);
		let mut buf = Vec::new();
		let mut sender = SecureSend::new(&mut buf, c2a);
		for chunk in [b"one".as_slice(), b"two", b"three"] {
			sender.write_all(chunk).await.unwrap();
		}
		sender.flush().await.unwrap();

		let mut reader = SecureRecv::new(OneByteAtATime(&buf[..], 0), c2a);
		let mut got = Vec::new();
		reader.read_to_end(&mut got).await.unwrap();
		assert_eq!(got, b"onetwothree");
	}

	/// A reader over a byte slice that returns at most one byte per `poll_read`, to
	/// stress the reassembly state machine.
	struct OneByteAtATime<'a>(&'a [u8], usize);

	impl AsyncRead for OneByteAtATime<'_> {
		fn poll_read(mut self: Pin<&mut Self>, _cx: &mut Context<'_>, out: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
			if self.1 < self.0.len() && out.remaining() > 0 {
				let b = self.0[self.1];
				self.1 += 1;
				out.put_slice(&[b]);
			}
			Poll::Ready(Ok(()))
		}
	}
}
