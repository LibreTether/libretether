//! On-disk agent configuration: where the controller is, the one-time
//! enrollment token (until consumed), and the agent's persistent identity seed.

use std::path::PathBuf;

use anyhow::{Context, Result};
use libretether_protocol::crypto::Identity;
use libretether_protocol::DEFAULT_PORT;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentConfig {
	/// `host:port` of the controller for direct mode (a tailnet name/IP). Empty
	/// when using relay mode.
	#[serde(default)]
	pub controller_addr: String,
	/// `host:port` of the relay (`libretether-relay`) for relay mode. When set, the
	/// agent dials the relay instead of the controller.
	#[serde(default)]
	pub relay_addr: Option<String>,
	/// Agent secret used to authenticate to the relay.
	#[serde(default)]
	pub relay_secret: Option<String>,
	/// TLS server name presented during the handshake.
	#[serde(default = "default_server_name")]
	pub server_name: String,
	/// One-time token, present until the agent has successfully enrolled.
	#[serde(default)]
	pub enrollment_token: Option<String>,
	/// Base64 Ed25519 public key of the controller this agent trusts, pinned from
	/// the deploy/enroll arguments. The agent rejects any controller whose
	/// key/signature don't match this; there is no trust-on-first-use, so an agent
	/// without it (e.g. an old config) must be re-enrolled. Optional only so an
	/// old config parses to a clear "re-enroll" error instead of a parse failure.
	#[serde(default)]
	pub controller_key: Option<String>,
	/// Base64 Ed25519 seed — the agent's stable identity.
	pub identity_seed: String,
	/// Controller-assigned id, learned at enrollment (informational).
	#[serde(default)]
	pub client_id: Option<String>,
}

fn default_server_name() -> String {
	"libretether.local".to_string()
}

impl AgentConfig {
	pub fn identity(&self) -> Result<Identity> {
		Identity::from_seed_b64(&self.identity_seed).context("config has an invalid identity_seed")
	}

	/// The relay address when in relay mode, else `None` (direct mode).
	pub fn relay(&self) -> Option<&str> {
		self.relay_addr.as_deref().filter(|s| !s.is_empty())
	}

	pub fn load(path: &PathBuf) -> Result<Self> {
		let raw =
			std::fs::read_to_string(path).with_context(|| format!("reading agent config at {}", path.display()))?;
		serde_json::from_str(&raw).context("parsing agent config")
	}

	pub fn save(&self, path: &PathBuf) -> Result<()> {
		// The config holds the identity seed, enrollment token and relay secret —
		// write it owner-only so other local users can't read them.
		let raw = serde_json::to_string_pretty(self)?;
		libretether_protocol::secret::write_str(path, &raw)
			.with_context(|| format!("writing agent config at {}", path.display()))?;
		Ok(())
	}
}

/// Default config path: `<config dir>/libretether-agent/config.json`.
pub fn default_config_path() -> PathBuf {
	dirs::config_dir()
		.unwrap_or_else(|| PathBuf::from("."))
		.join("libretether-agent")
		.join("config.json")
}

/// Normalize a controller address, defaulting the port when omitted.
pub fn normalize_addr(addr: &str) -> String {
	if addr.contains(':') {
		addr.to_string()
	} else {
		format!("{addr}:{DEFAULT_PORT}")
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn appends_default_port_when_missing() {
		assert_eq!(normalize_addr("ctl.example"), format!("ctl.example:{DEFAULT_PORT}"));
		assert_eq!(normalize_addr("10.0.0.5"), format!("10.0.0.5:{DEFAULT_PORT}"));
	}

	#[test]
	fn keeps_an_explicit_port() {
		assert_eq!(normalize_addr("ctl.example:1234"), "ctl.example:1234");
		assert_eq!(normalize_addr("10.0.0.5:9000"), "10.0.0.5:9000");
	}

	#[test]
	fn relay_mode_is_detected_from_a_non_empty_relay_addr() {
		let mut cfg = AgentConfig {
			controller_addr: String::new(),
			relay_addr: Some("relay.example:47600".into()),
			relay_secret: Some("s".into()),
			server_name: default_server_name(),
			enrollment_token: None,
			controller_key: Some("k".into()),
			identity_seed: Identity::generate().seed_b64(),
			client_id: None,
		};
		assert_eq!(cfg.relay(), Some("relay.example:47600"));
		// An empty relay address falls back to direct mode.
		cfg.relay_addr = Some(String::new());
		assert_eq!(cfg.relay(), None);
		cfg.relay_addr = None;
		assert_eq!(cfg.relay(), None);
	}
}
