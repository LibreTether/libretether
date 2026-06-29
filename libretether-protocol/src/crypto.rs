//! Ed25519 identity for agents. The private seed never leaves the agent; the
//! controller only ever stores the public key it sees at enrollment and uses it
//! to verify the signature over each connection's challenge nonce.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

/// An agent's signing identity, backed by an Ed25519 keypair.
pub struct Identity {
	signing: SigningKey,
}

/// Fill `buf` with cryptographically secure random bytes from the OS.
///
/// The single place the OS RNG is touched, so the failure policy lives in one
/// spot. A failure here is treated as fatal and fail-closed: a security tool must
/// never proceed with weak/absent randomness for identities, nonces, tokens or
/// secrets, and `getrandom` only fails in genuinely broken environments (no
/// `/dev/urandom`, an exhausted fd table, a seccomp sandbox that blocks the
/// syscall) where there is no safe way to continue.
pub fn fill_random(buf: &mut [u8]) {
	getrandom::getrandom(buf).expect("os rng unavailable — cannot generate secure randomness");
}

impl Identity {
	/// Generate a brand-new random identity.
	pub fn generate() -> Self {
		let mut seed = [0u8; 32];
		fill_random(&mut seed);
		Self {
			signing: SigningKey::from_bytes(&seed),
		}
	}

	/// Restore an identity from the base64 32-byte seed produced by [`Self::seed_b64`].
	pub fn from_seed_b64(seed: &str) -> Option<Self> {
		let bytes = B64.decode(seed).ok()?;
		let arr: [u8; 32] = bytes.try_into().ok()?;
		Some(Self {
			signing: SigningKey::from_bytes(&arr),
		})
	}

	/// The base64 seed — persist this (and only this) to disk.
	pub fn seed_b64(&self) -> String {
		B64.encode(self.signing.to_bytes())
	}

	/// The base64 public key shared with the controller.
	pub fn public_b64(&self) -> String {
		B64.encode(self.signing.verifying_key().to_bytes())
	}

	/// Sign an arbitrary message, returning a base64 signature.
	pub fn sign_b64(&self, msg: &[u8]) -> String {
		B64.encode(self.signing.sign(msg).to_bytes())
	}
}

/// A fresh random 32-byte nonce, base64-encoded — used as a per-connection challenge.
pub fn random_nonce_b64() -> String {
	let mut nonce = [0u8; 32];
	fill_random(&mut nonce);
	B64.encode(nonce)
}

/// A random alphanumeric string of `len` characters (e.g. for RDP passwords and
/// relay secrets). Uses rejection sampling so every symbol is equally likely —
/// a plain `byte % len` would bias toward the first `256 % len` symbols.
pub fn random_alnum(len: usize) -> String {
	const CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";
	// Largest multiple of CHARS.len() that fits in a u8; bytes at or above it are
	// rejected so the modulo is unbiased.
	let limit = (256 / CHARS.len() * CHARS.len()) as u8;
	let mut out = String::with_capacity(len);
	let mut buf = [0u8; 64];
	while out.len() < len {
		fill_random(&mut buf);
		for &b in &buf {
			if b < limit {
				out.push(CHARS[b as usize % CHARS.len()] as char);
				if out.len() == len {
					break;
				}
			}
		}
	}
	out
}

/// Constant-time string equality, for comparing secrets and tokens without
/// leaking a match prefix through early-exit timing. (Length is allowed to leak;
/// our secrets/tokens are fixed-length.)
pub fn ct_eq(a: &str, b: &str) -> bool {
	use subtle::ConstantTimeEq;
	let (a, b) = (a.as_bytes(), b.as_bytes());
	a.len() == b.len() && a.ct_eq(b).into()
}

/// Verify that `sig_b64` is a valid signature of `msg` under `public_b64`.
pub fn verify_b64(public_b64: &str, msg: &[u8], sig_b64: &str) -> bool {
	let Some(vk) = decode_pubkey(public_b64) else {
		return false;
	};
	let Ok(sig_bytes) = B64.decode(sig_b64) else {
		return false;
	};
	let Ok(sig_arr): Result<[u8; 64], _> = sig_bytes.try_into() else {
		return false;
	};
	// `verify_strict` rejects malleable signatures and small-order public keys.
	vk.verify_strict(msg, &Signature::from_bytes(&sig_arr)).is_ok()
}

/// Decode a base64 string to exactly `N` bytes, or `None` if it isn't valid
/// base64 of that length. Shared by the public-key and canonicalization paths so
/// the fixed-length decode lives in one spot.
fn decode_fixed<const N: usize>(b64: &str) -> Option<[u8; N]> {
	B64.decode(b64.trim()).ok()?.try_into().ok()
}

fn decode_pubkey(public_b64: &str) -> Option<VerifyingKey> {
	VerifyingKey::from_bytes(&decode_fixed::<32>(public_b64)?).ok()
}

/// Canonicalize a base64 Ed25519 public key: decode to the 32 raw key bytes and
/// re-encode. Two different base64 encodings of the same key (e.g. trailing
/// whitespace, or a non-canonical encoding accepted on decode) collapse to one
/// string, so it can be used as a stable routing/identity key. Returns `None` if
/// the input isn't a valid 32-byte key.
pub fn canonical_pubkey(public_b64: &str) -> Option<String> {
	Some(B64.encode(decode_fixed::<32>(public_b64)?))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn sign_verify_round_trip() {
		let id = Identity::generate();
		let sig = id.sign_b64(b"hello");
		assert!(verify_b64(&id.public_b64(), b"hello", &sig));
	}

	#[test]
	fn verify_rejects_tampered_message_wrong_key_and_garbage() {
		let id = Identity::generate();
		let other = Identity::generate();
		let sig = id.sign_b64(b"hello");
		// Right key, wrong message.
		assert!(!verify_b64(&id.public_b64(), b"goodbye", &sig));
		// Valid signature but a different signer's key.
		assert!(!verify_b64(&other.public_b64(), b"hello", &sig));
		// Malformed inputs never panic and never verify.
		assert!(!verify_b64("not base64!!", b"hello", &sig));
		assert!(!verify_b64(&id.public_b64(), b"hello", "not base64!!"));
		assert!(!verify_b64(&id.public_b64(), b"hello", &B64.encode([0u8; 64])));
	}

	// An empty key or empty signature must never verify. This is what makes the
	// handshake fail closed if a peer omits the mutual-auth fields (the agent
	// compares against a non-empty pinned key, and verifies a real signature).
	#[test]
	fn verify_rejects_empty_key_or_signature() {
		let id = Identity::generate();
		let sig = id.sign_b64(b"msg");
		assert!(!verify_b64("", b"msg", &sig));
		assert!(!verify_b64(&id.public_b64(), b"msg", ""));
		assert!(!verify_b64("", b"msg", ""));
	}

	#[test]
	fn seed_round_trip_preserves_identity() {
		let id = Identity::generate();
		let restored = Identity::from_seed_b64(&id.seed_b64()).expect("valid seed");
		assert_eq!(id.public_b64(), restored.public_b64());
		// A signature from the restored key verifies under the original public key.
		assert!(verify_b64(&id.public_b64(), b"x", &restored.sign_b64(b"x")));
		assert!(Identity::from_seed_b64("nonsense").is_none());
	}

	#[test]
	fn ct_eq_matches_only_equal_strings() {
		assert!(ct_eq("secret", "secret"));
		assert!(!ct_eq("secret", "secres"));
		assert!(!ct_eq("secret", "secret "));
		assert!(!ct_eq("", "x"));
		assert!(ct_eq("", ""));
	}

	#[test]
	fn canonical_pubkey_normalizes_and_rejects_invalid_keys() {
		let id = Identity::generate();
		let pk = id.public_b64();
		// A canonical key round-trips to itself.
		assert_eq!(canonical_pubkey(&pk).as_deref(), Some(pk.as_str()));
		// Surrounding whitespace collapses to the same canonical string, so two
		// encodings of the same key can't register/route as distinct peers.
		assert_eq!(canonical_pubkey(&format!("  {pk}  ")).as_deref(), Some(pk.as_str()));
		// Wrong length and non-base64 are rejected.
		assert!(canonical_pubkey(&B64.encode([0u8; 31])).is_none());
		assert!(canonical_pubkey("not base64!!").is_none());
		assert!(canonical_pubkey("").is_none());
	}

	#[test]
	fn random_alnum_has_the_right_length_and_alphabet() {
		const CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";
		for len in [0usize, 1, 16, 24, 100] {
			let s = random_alnum(len);
			assert_eq!(s.len(), len);
			assert!(s.bytes().all(|b| CHARS.contains(&b)), "unexpected char in {s:?}");
		}
	}
}
