//! Best-effort Tailscale detection so the controller can suggest the address
//! agents should dial, and surface tailnet status in the UI.

use serde::Serialize;
use tokio::process::Command;

#[derive(Debug, Clone, Serialize)]
pub struct TailscaleInfo {
	pub installed: bool,
	pub running: bool,
	/// MagicDNS name (preferred) or 100.x address agents should dial.
	pub address: Option<String>,
	pub hostname: Option<String>,
}

impl TailscaleInfo {
	fn unavailable() -> Self {
		Self {
			installed: false,
			running: false,
			address: None,
			hostname: None,
		}
	}
}

/// Query the local Tailscale daemon. Never fails — returns an "unavailable"
/// record when Tailscale isn't installed or reachable.
pub async fn status() -> TailscaleInfo {
	let Some(bin) = locate().await else {
		return TailscaleInfo::unavailable();
	};

	let output = Command::new(&bin).args(["status", "--json"]).output().await;
	let Ok(output) = output else {
		return TailscaleInfo {
			installed: true,
			..TailscaleInfo::unavailable()
		};
	};
	if !output.status.success() {
		return TailscaleInfo {
			installed: true,
			..TailscaleInfo::unavailable()
		};
	}

	let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
		return TailscaleInfo {
			installed: true,
			..TailscaleInfo::unavailable()
		};
	};

	let running = json.get("BackendState").and_then(|v| v.as_str()) == Some("Running");
	let self_node = json.get("Self");
	let hostname = self_node
		.and_then(|s| s.get("HostName"))
		.and_then(|v| v.as_str())
		.map(str::to_string);

	// Prefer MagicDNS name (trailing dot trimmed), else the first 100.x IP.
	let dns = self_node
		.and_then(|s| s.get("DNSName"))
		.and_then(|v| v.as_str())
		.map(|s| s.trim_end_matches('.').to_string())
		.filter(|s| !s.is_empty());
	let ipv4 = self_node
		.and_then(|s| s.get("TailscaleIPs"))
		.and_then(|v| v.as_array())
		.and_then(|arr| arr.iter().find_map(|v| v.as_str().filter(|s| s.contains('.'))))
		.map(str::to_string);

	TailscaleInfo {
		installed: true,
		running,
		address: dns.or(ipv4),
		hostname,
	}
}

/// Find the `tailscale` CLI on PATH or in the common macOS app bundle location.
async fn locate() -> Option<String> {
	for candidate in ["tailscale", "/Applications/Tailscale.app/Contents/MacOS/Tailscale"] {
		if Command::new(candidate)
			.arg("version")
			.output()
			.await
			.map(|o| o.status.success())
			.unwrap_or(false)
		{
			return Some(candidate.to_string());
		}
	}
	None
}
