//! A 64-word list for rendering a pairing's verify phrase as spoken words.
//!
//! The verify phrase is a human cross-check that both ends of a pairing derived
//! the *same* PAKE key (and therefore that no one is in the middle and the right
//! machine paired). 64 short, phonetically distinct words means each word encodes
//! exactly 6 bits with no modulo bias (256 mod 64 == 0, so `byte % 64` is uniform);
//! [`crate::pairing`] renders four of them for ~24 bits of cross-check.
//!
//! The words are deliberately common, easy to say over a phone, and chosen to
//! avoid homophones and easily-confused pairs.

/// The 64 words. Index with a byte value masked to 6 bits (`b & 0x3f`).
pub const WORDS: [&str; 64] = [
	"amber", "anchor", "apple", "arrow", "autumn", "bamboo", "beacon", "birch", "bishop", "bridge", "cabin", "cactus",
	"canyon", "cedar", "cobalt", "comet", "copper", "coral", "cotton", "crane", "delta", "ember", "falcon", "fern",
	"forest", "garnet", "ginger", "granite", "harbor", "hazel", "heron", "indigo", "ivory", "jade", "jasmine",
	"jungle", "kettle", "lagoon", "lantern", "lemon", "lily", "lotus", "maple", "marble", "meadow", "mango", "maroon",
	"nectar", "nutmeg", "olive", "onyx", "orchid", "otter", "panda", "pebble", "pepper", "pewter", "quartz", "quail",
	"raven", "river", "saffron", "violet", "willow",
];

#[cfg(test)]
mod tests {
	use super::*;
	use std::collections::HashSet;

	#[test]
	fn has_sixty_four_distinct_words() {
		assert_eq!(WORDS.len(), 64);
		let unique: HashSet<&str> = WORDS.iter().copied().collect();
		assert_eq!(
			unique.len(),
			64,
			"every word must be distinct so the phrase is unambiguous"
		);
	}

	#[test]
	fn words_are_lowercase_ascii_letters_only() {
		// Spoken over a phone and typed back: keep them simple so there's nothing to
		// mis-hear or mis-key (no digits, punctuation, or mixed case).
		for w in WORDS {
			assert!(!w.is_empty());
			assert!(
				w.bytes().all(|b| b.is_ascii_lowercase()),
				"word {w:?} must be lowercase ascii letters only"
			);
		}
	}
}
