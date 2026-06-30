//! Phone-friendly agent pairing: a short spoken code carries enrollment over an
//! untrusted relay without dictating any key material.
//!
//! The operator reads a short code aloud; the new machine types it. The code
//! splits into a **nameplate** (the relay-visible routing id that matches the two
//! parties) and a **password** (a SPAKE2 secret the relay never sees). Controller
//! and agent run a [SPAKE2](https://datatracker.ietf.org/doc/html/draft-irtf-cfrg-spake2)
//! PAKE over the code, deriving a shared key only if they used the same code; the
//! controller then sends the enrollment [`PairBundle`] AEAD-sealed under that key.
//!
//! Why this is safe over a relay that only forwards bytes (see
//! [`crate::relay`]): the relay learns only the nameplate, never the password or
//! the bundle. It cannot read the bundle (it's encrypted under the PAKE key) and
//! cannot machine-in-the-middle it — without the password its SPAKE2 contribution
//! yields a different key, so key confirmation fails and the controller never
//! reveals the bundle. A wrong code gets exactly **one** online guess per slot
//! (then the slot is burned), which is why a short code is enough.
//!
//! The flow over the (relay-piped) byte channel:
//! 1. both sides send their SPAKE2 message and derive a key;
//! 2. the **agent** proves it derived the same key (a key-confirmation MAC) so the
//!    controller never hands the bundle to a wrong-code peer;
//! 3. the **controller** sends the AEAD-sealed bundle — a successful open is in
//!    turn the controller's proof that it held the key.
//!
//! Both ends also derive an identical [`PakeKeys::verify_phrase`] from the key,
//! shown to the human as a last cross-check that the *right* machine paired.

use std::io;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::crypto::{ct_eq, random_alnum};
use crate::frame::{read_frame_capped, write_frame, MAX_CONTROL_FRAME};
use crate::wordlist::WORDS;

/// Characters in a dictatable code: the unambiguous alphabet from
/// [`random_alnum`] (no `0/O/1/l/I`), so a code can't be mis-heard or mis-typed.
const NAMEPLATE_LEN: usize = 4;
const PASSWORD_LEN: usize = 4;

/// Domain separator mixed into the SPAKE2 identity, bumped if the pairing wire
/// format changes (paired with the controller/agent shipping together).
const PAIRING_DOMAIN: &str = "libretether-pairing-v1";

fn invalid(msg: impl Into<String>) -> io::Error {
	io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// The enrollment payload the controller hands a new machine once paired. It
/// carries exactly what the agent needs to enroll in relay mode — the agent
/// already knows the relay address (it dialed the relay to pair).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairBundle {
	/// One-time enrollment token (consumed on first connect).
	pub enrollment_token: String,
	/// Base64 Ed25519 public key the agent must pin (the `controller_key`).
	pub controller_key: String,
	/// The relay's agent secret, so the agent can authenticate to the relay.
	pub agent_secret: String,
	/// The human-facing name the controller registered, for the agent's logs.
	#[serde(default)]
	pub name: Option<String>,
}

/// A pairing code split into its two halves. The full dictated form is
/// `nameplate-password`; only the nameplate is ever sent to the relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCode {
	pub nameplate: String,
	pub password: String,
}

impl PairingCode {
	/// A fresh random code. The nameplate routes the two parties at the relay; the
	/// password is the SPAKE2 secret that authenticates them to each other.
	pub fn generate() -> Self {
		Self {
			nameplate: random_alnum(NAMEPLATE_LEN),
			password: random_alnum(PASSWORD_LEN),
		}
	}

	/// The dictatable form, `nameplate-password` (e.g. `4F9K-2A7C`).
	pub fn full(&self) -> String {
		format!("{}-{}", self.nameplate, self.password)
	}

	/// Parse a dictated code. Tolerant of surrounding whitespace and case in the
	/// separator only — the alphabet itself is case-sensitive, so the two halves are
	/// taken verbatim. Returns `None` if it isn't exactly `nameplate-password` with
	/// both halves non-empty.
	pub fn parse(s: &str) -> Option<Self> {
		let s = s.trim();
		let (nameplate, password) = s.split_once('-')?;
		if nameplate.is_empty() || password.is_empty() || password.contains('-') {
			return None;
		}
		Some(Self {
			nameplate: nameplate.to_string(),
			password: password.to_string(),
		})
	}
}

/// In-progress SPAKE2 state, between sending our message and receiving the peer's.
pub struct PakeState(Spake2<Ed25519Group>);

/// Start the PAKE: returns our state plus the SPAKE2 message to send the peer.
/// `nameplate` binds the exchange to this specific slot (domain separation), so a
/// password reused across slots can't be cross-matched.
pub fn pake_start(code: &PairingCode) -> (PakeState, Vec<u8>) {
	let identity = format!("{PAIRING_DOMAIN}:{}", code.nameplate);
	let (state, outbound) = Spake2::<Ed25519Group>::start_symmetric(
		&Password::new(code.password.as_bytes()),
		&SpakeIdentity::new(identity.as_bytes()),
	);
	(PakeState(state), outbound)
}

impl PakeState {
	/// Finish the PAKE with the peer's message, deriving the shared keys. A wrong
	/// code does not error here — SPAKE2 yields a *different* key, detected later by
	/// key confirmation. This only errors if the peer's message is malformed.
	pub fn finish(self, peer_msg: &[u8]) -> io::Result<PakeKeys> {
		let raw = self
			.0
			.finish(peer_msg)
			.map_err(|e| invalid(format!("pairing handshake failed: {e}")))?;
		// Run the raw SPAKE2 output through HKDF so every sub-key is domain-separated
		// and fixed-length regardless of the group's output encoding.
		let mut master = [0u8; 32];
		Hkdf::<Sha256>::new(Some(PAIRING_DOMAIN.as_bytes()), &raw)
			.expand(b"master", &mut master)
			.expect("hkdf master within length");
		Ok(PakeKeys { master })
	}
}

/// The keys derived from a completed PAKE. Identical on both ends iff the same code
/// was used. All sub-keys are HKDF-derived from one master with distinct labels.
pub struct PakeKeys {
	master: [u8; 32],
}

impl PakeKeys {
	fn derive<const N: usize>(&self, label: &[u8]) -> [u8; N] {
		let mut out = [0u8; N];
		Hkdf::<Sha256>::new(None, &self.master)
			.expand(label, &mut out)
			.expect("hkdf sub-key within length");
		out
	}

	/// The agent's key-confirmation MAC: the agent sends this so the controller can
	/// confirm the agent derived the same key *before* revealing the bundle.
	pub fn agent_confirmation(&self) -> [u8; 32] {
		self.derive(b"agent-confirm")
	}

	/// A short spoken phrase (four words) both ends derive identically. A human
	/// cross-check that no one is in the middle and the right machine paired.
	pub fn verify_phrase(&self) -> String {
		let bytes: [u8; 4] = self.derive(b"verify-phrase");
		bytes
			.iter()
			.map(|b| WORDS[(b & 0x3f) as usize])
			.collect::<Vec<_>>()
			.join("-")
	}

	fn cipher(&self) -> (ChaCha20Poly1305, Nonce) {
		let key = Key::from(self.derive::<32>(b"bundle-key"));
		let nonce = Nonce::from(self.derive::<12>(b"bundle-nonce"));
		(ChaCha20Poly1305::new(&key), nonce)
	}

	/// AEAD-seal the bundle under the PAKE key. The key is single-use, so a derived
	/// fixed nonce is safe (it's never reused with this key).
	pub fn seal_bundle(&self, bundle: &PairBundle) -> io::Result<Vec<u8>> {
		let plaintext = serde_json::to_vec(bundle).map_err(|e| invalid(e.to_string()))?;
		let (cipher, nonce) = self.cipher();
		cipher
			.encrypt(&nonce, plaintext.as_ref())
			.map_err(|_| invalid("sealing the pairing bundle failed"))
	}

	/// Open a sealed bundle. A failure here means the sealer did not hold the same
	/// key (wrong code, or tampering) — so a successful open authenticates the
	/// controller to the agent.
	pub fn open_bundle(&self, sealed: &[u8]) -> io::Result<PairBundle> {
		let (cipher, nonce) = self.cipher();
		let plaintext = cipher
			.decrypt(&nonce, sealed)
			.map_err(|_| invalid("pairing failed: wrong code or tampered bundle"))?;
		serde_json::from_slice(&plaintext).map_err(|e| invalid(e.to_string()))
	}
}

// ----------------------------------------------------------------- wire frames

#[derive(Serialize, Deserialize)]
struct PakeMsg {
	/// Base64 SPAKE2 message.
	msg: String,
}

#[derive(Serialize, Deserialize)]
struct PakeConfirm {
	/// Base64 key-confirmation MAC.
	mac: String,
}

#[derive(Serialize, Deserialize)]
struct PakeSealed {
	/// Base64 AEAD-sealed [`PairBundle`].
	ciphertext: String,
}

// ----------------------------------------------------------- exchange helpers

/// Run the controller's side of a pairing over a (relay-piped) byte channel:
/// exchange SPAKE2 messages, require the agent's key confirmation, then send the
/// sealed bundle. Returns the verify phrase to show the operator.
pub async fn controller_pair<W, R>(
	send: &mut W,
	recv: &mut R,
	code: &PairingCode,
	bundle: &PairBundle,
) -> io::Result<String>
where
	W: AsyncWrite + Unpin,
	R: AsyncRead + Unpin,
{
	let (state, outbound) = pake_start(code);
	write_frame(
		send,
		&PakeMsg {
			msg: B64.encode(&outbound),
		},
	)
	.await?;
	let peer: PakeMsg = read_frame_capped(recv, MAX_CONTROL_FRAME).await?;
	let peer_msg = B64.decode(&peer.msg).map_err(|e| invalid(e.to_string()))?;
	let keys = state.finish(&peer_msg)?;

	// Require the agent's key confirmation before revealing anything: a wrong-code
	// peer (or the relay guessing) must not receive the sealed bundle to attack.
	let confirm: PakeConfirm = read_frame_capped(recv, MAX_CONTROL_FRAME).await?;
	let expected = B64.encode(keys.agent_confirmation());
	if !ct_eq(&confirm.mac, &expected) {
		return Err(invalid(
			"pairing failed: wrong code or tampering (key confirmation mismatch)",
		));
	}

	let sealed = keys.seal_bundle(bundle)?;
	write_frame(
		send,
		&PakeSealed {
			ciphertext: B64.encode(&sealed),
		},
	)
	.await?;
	Ok(keys.verify_phrase())
}

/// Run the agent's side: exchange SPAKE2 messages, send key confirmation, then
/// open the sealed bundle (a successful open authenticates the controller).
/// Returns the bundle and the verify phrase to show the operator.
pub async fn agent_pair<W, R>(send: &mut W, recv: &mut R, code: &PairingCode) -> io::Result<(PairBundle, String)>
where
	W: AsyncWrite + Unpin,
	R: AsyncRead + Unpin,
{
	let (state, outbound) = pake_start(code);
	write_frame(
		send,
		&PakeMsg {
			msg: B64.encode(&outbound),
		},
	)
	.await?;
	let peer: PakeMsg = read_frame_capped(recv, MAX_CONTROL_FRAME).await?;
	let peer_msg = B64.decode(&peer.msg).map_err(|e| invalid(e.to_string()))?;
	let keys = state.finish(&peer_msg)?;

	write_frame(
		send,
		&PakeConfirm {
			mac: B64.encode(keys.agent_confirmation()),
		},
	)
	.await?;
	let sealed: PakeSealed = read_frame_capped(recv, MAX_CONTROL_FRAME).await?;
	let ciphertext = B64.decode(&sealed.ciphertext).map_err(|e| invalid(e.to_string()))?;
	let bundle = keys.open_bundle(&ciphertext)?;
	Ok((bundle, keys.verify_phrase()))
}

#[cfg(test)]
mod tests {
	use super::*;
	use tokio::io::split;

	fn sample_bundle() -> PairBundle {
		PairBundle {
			enrollment_token: "tok-abc123".into(),
			controller_key: "Q29udHJvbGxlcktleUJhc2U2NEV4YW1wbGVQYWRkaW5n".into(),
			agent_secret: "agent-sekret".into(),
			name: Some("office-imac".into()),
		}
	}

	#[test]
	fn code_round_trips_through_its_full_form() {
		let code = PairingCode::generate();
		assert_eq!(code.nameplate.len(), NAMEPLATE_LEN);
		assert_eq!(code.password.len(), PASSWORD_LEN);
		let parsed = PairingCode::parse(&code.full()).expect("round-trips");
		assert_eq!(parsed, code);
		// Tolerates surrounding whitespace.
		assert_eq!(PairingCode::parse(&format!("  {}  ", code.full())), Some(code));
	}

	#[test]
	fn code_uses_the_unambiguous_alphabet() {
		// No characters that are easy to mis-hear/mis-type when read aloud.
		const AMBIGUOUS: &[char] = &['0', 'O', '1', 'l', 'I'];
		for _ in 0..200 {
			let code = PairingCode::generate();
			for c in code.nameplate.chars().chain(code.password.chars()) {
				assert!(
					!AMBIGUOUS.contains(&c),
					"code {} has an ambiguous char {c}",
					code.full()
				);
				assert!(c.is_ascii_alphanumeric());
			}
		}
	}

	#[test]
	fn code_parse_rejects_malformed_input() {
		assert!(PairingCode::parse("nodelimiter").is_none());
		assert!(PairingCode::parse("-pw").is_none());
		assert!(PairingCode::parse("np-").is_none());
		assert!(PairingCode::parse("").is_none());
		// A third segment is ambiguous — reject rather than guess.
		assert!(PairingCode::parse("a-b-c").is_none());
	}

	#[test]
	fn matching_codes_derive_an_identical_verify_phrase_and_key() {
		// Both ends run the symmetric PAKE with the same code and must converge.
		let code = PairingCode::generate();
		let (a_state, a_msg) = pake_start(&code);
		let (b_state, b_msg) = pake_start(&code);
		let a_keys = a_state.finish(&b_msg).unwrap();
		let b_keys = b_state.finish(&a_msg).unwrap();
		assert_eq!(a_keys.master, b_keys.master, "same code → same key");
		assert_eq!(a_keys.verify_phrase(), b_keys.verify_phrase());
		// The phrase is four words from the wordlist.
		let phrase = a_keys.verify_phrase();
		let words: Vec<&str> = phrase.split('-').collect();
		assert_eq!(words.len(), 4);
		assert!(words.iter().all(|w| WORDS.contains(w)));
	}

	#[test]
	fn mismatched_codes_derive_different_keys() {
		// Different passwords → SPAKE2 finishes (no error) but yields different keys,
		// which is what key confirmation / AEAD later turns into a clean failure.
		let a = PairingCode {
			nameplate: "ABCD".into(),
			password: "RIGHT".into(),
		};
		let b = PairingCode {
			nameplate: "ABCD".into(),
			password: "WRONG".into(),
		};
		let (a_state, a_msg) = pake_start(&a);
		let (b_state, b_msg) = pake_start(&b);
		let a_keys = a_state.finish(&b_msg).unwrap();
		let b_keys = b_state.finish(&a_msg).unwrap();
		assert_ne!(a_keys.master, b_keys.master);
		assert_ne!(a_keys.verify_phrase(), b_keys.verify_phrase());
	}

	#[test]
	fn seal_open_round_trips_under_the_same_key() {
		let code = PairingCode::generate();
		let (a_state, a_msg) = pake_start(&code);
		let (b_state, b_msg) = pake_start(&code);
		let a_keys = a_state.finish(&b_msg).unwrap();
		let b_keys = b_state.finish(&a_msg).unwrap();
		let bundle = sample_bundle();
		let sealed = a_keys.seal_bundle(&bundle).unwrap();
		assert_eq!(b_keys.open_bundle(&sealed).unwrap(), bundle);
	}

	#[test]
	fn open_fails_under_a_different_key() {
		let bundle = sample_bundle();
		let right = PairingCode::generate();
		let (rs, rm) = pake_start(&right);
		let (rs2, rm2) = pake_start(&right);
		let sealer = rs.finish(&rm2).unwrap();
		let _ = rs2.finish(&rm).unwrap();
		let sealed = sealer.seal_bundle(&bundle).unwrap();

		// A party that completed a *different* PAKE can't open it.
		let wrong = PairingCode::generate();
		let (ws, wm) = pake_start(&wrong);
		let (ws2, wm2) = pake_start(&wrong);
		let opener = ws.finish(&wm2).unwrap();
		let _ = ws2.finish(&wm).unwrap();
		assert!(opener.open_bundle(&sealed).is_err());
	}

	#[test]
	fn open_fails_on_a_tampered_ciphertext() {
		let code = PairingCode::generate();
		let (a, am) = pake_start(&code);
		let (b, bm) = pake_start(&code);
		let ak = a.finish(&bm).unwrap();
		let bk = b.finish(&am).unwrap();
		let mut sealed = ak.seal_bundle(&sample_bundle()).unwrap();
		*sealed.last_mut().unwrap() ^= 0x01; // flip a bit in the AEAD tag
		assert!(bk.open_bundle(&sealed).is_err(), "AEAD must reject tampering");
	}

	/// Wire the two ends together over an in-memory duplex and run the full
	/// async exchange concurrently.
	async fn run_exchange(
		ctrl_code: &PairingCode,
		agent_code: &PairingCode,
		bundle: &PairBundle,
	) -> (io::Result<String>, io::Result<(PairBundle, String)>) {
		let (ctrl_io, agent_io) = tokio::io::duplex(8192);
		let (mut ctrl_r, mut ctrl_w) = split(ctrl_io);
		let (mut agent_r, mut agent_w) = split(agent_io);
		let ctrl = {
			let code = ctrl_code.clone();
			let bundle = bundle.clone();
			async move { controller_pair(&mut ctrl_w, &mut ctrl_r, &code, &bundle).await }
		};
		let agent = {
			let code = agent_code.clone();
			async move { agent_pair(&mut agent_w, &mut agent_r, &code).await }
		};
		tokio::join!(ctrl, agent)
	}

	#[tokio::test]
	async fn full_exchange_delivers_the_bundle_and_matching_phrase() {
		let code = PairingCode::generate();
		let bundle = sample_bundle();
		let (ctrl, agent) = run_exchange(&code, &code, &bundle).await;
		let ctrl_phrase = ctrl.expect("controller side ok");
		let (got, agent_phrase) = agent.expect("agent side ok");
		assert_eq!(got, bundle, "agent receives the exact bundle");
		assert_eq!(ctrl_phrase, agent_phrase, "both ends show the same verify phrase");
	}

	#[tokio::test]
	async fn full_exchange_fails_closed_on_a_wrong_code() {
		// The agent types a wrong password. The controller must reject at key
		// confirmation (never sending the bundle), and the agent must error too.
		let ctrl_code = PairingCode {
			nameplate: "ABCD".into(),
			password: "RIGHT".into(),
		};
		let agent_code = PairingCode {
			nameplate: "ABCD".into(),
			password: "WRONG".into(),
		};
		let (ctrl, agent) = run_exchange(&ctrl_code, &agent_code, &sample_bundle()).await;
		assert!(
			ctrl.is_err(),
			"controller must not reveal the bundle to a wrong-code peer"
		);
		assert!(agent.is_err(), "agent must fail closed on a wrong code");
	}
}
