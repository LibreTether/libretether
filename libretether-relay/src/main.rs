//! LibreTether relay (`libretether-relay`).
//!
//! Run this on a public cloud host. It is **multi-tenant**: it holds a set of
//! independent tenants, each with its own owner secret (its controller) and agent
//! secret (its agents). The controller and the agents all dial out to it; the
//! secret each presents identifies its tenant, and the relay tracks that tenant's
//! agents by Ed25519 public key and pipes streams between the tenant's controller
//! and the addressed agent. Routing is isolated per tenant — a controller only ever
//! sees the agents that dialed in under its own agent secret. It never inspects
//! stream contents: the LibreTether handshake, control RPCs, live session and TCP
//! tunnels are all end-to-end.
//!
//! Tenants are minted over an admin channel gated by the relay's **admin secret**
//! (or open to any client when `open_registration` is set): a client provisions a
//! tenant and receives its freshly-generated owner/agent secrets. Tenants persist in
//! the on-disk config so they survive restarts.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use clap::{Parser, Subcommand};
use libretether_common::{pipe_bidirectional, shutdown_signal};
use libretether_protocol::crypto::{self, random_alnum};
use libretether_protocol::frame::{read_frame_capped, write_frame, MAX_CONTROL_FRAME};
use libretether_protocol::relay::{
	AdminRequest, AdminResponse, PunchResponse, RelayAck, RelayChallenge, RelayEvent, RelayHello, RelayProof,
	RelayRequest, RelayRole, RelaySignal, TenantCredentials, TenantInfo,
};
use libretether_protocol::{secret, tls, DEFAULT_PORT};
use quinn::{RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

mod logbook;
mod portal;

/// First 8 characters of a public key, for log lines (the full key is long and
/// noisy; the prefix is enough to correlate an agent across log entries).
fn key8(public_key: &str) -> String {
	public_key.chars().take(8).collect()
}

/// Hard ceiling on concurrent connections we'll service at once — beyond this we
/// shed load rather than spawn unbounded tasks for a UDP-reachable public port.
const MAX_CONNECTIONS: usize = 1024;
/// Slots within [`MAX_CONNECTIONS`] kept out of reach of agents, so the controller
/// can always connect even while an agent-secret holder opens connections in bulk.
/// Agents acquire from a pool of `MAX_CONNECTIONS - CONTROLLER_RESERVED`; the
/// global semaphore (held from accept, before the role is known) still bounds the
/// total, so authenticated agents can occupy at most that many long-lived slots,
/// leaving this much headroom for the controller and in-flight handshakes.
const CONTROLLER_RESERVED: usize = 16;
/// How long a peer has to complete the QUIC handshake *and* the auth handshake
/// before we drop it, so a peer that connects and then stalls at either stage
/// can't tie up a connection slot.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the relay emits an application-level heartbeat to the connected
/// controller, so a wedged relay (QUIC still answering keep-alives, routing loop
/// stuck) is detected by the controller's read timeout. Must be well under the
/// controller's read timeout.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// Per-source rate limit: at most this many new connections per IP per window.
const RATE_LIMIT_PER_WINDOW: u32 = 60;
const RATE_WINDOW: Duration = Duration::from_secs(10);
/// QUIC application error code the relay resets a routed stream with when the
/// addressed agent isn't connected, so the controller can tell "agent offline"
/// apart from a transport drop.
const AGENT_UNAVAILABLE: u32 = 0x10;
/// QUIC application error code the relay resets a pairing-join stream with when no
/// open slot matches its nameplate, so the joining machine gets an attributable
/// "no such pairing" rather than an opaque close.
const PAIRING_UNAVAILABLE: u32 = 0x11;
/// How long a controller's open pairing slot waits for a machine to join before the
/// relay drops it. A phone-driven install completes in well under this; a slot left
/// dangling (operator closed the dialog) is swept rather than parked forever.
const PAIRING_TTL: Duration = Duration::from_secs(300);
/// Ceiling on simultaneously-parked pairing slots, so a controller can't grow the
/// map without bound. Far above any real fan-out (one operator pairs one machine at
/// a time); expired slots are swept first when this is hit.
const MAX_PENDING_PAIRINGS: usize = 64;
/// How often the relay sweeps expired pairing slots.
const PAIRING_SWEEP_INTERVAL: Duration = Duration::from_secs(30);
/// Hard ceiling on provisioned tenants, so a compromised admin secret (or an
/// abusive open-registration client) can't grow the persisted config without bound.
/// Far above any realistic shared relay.
const MAX_TENANTS: usize = 512;
/// Length of a generated tenant id and of the owner/agent/admin secrets.
const SECRET_LEN: usize = 24;
const TENANT_ID_LEN: usize = 12;

/// A provisioned tenant as it lives on disk: its id, label, and the owner/agent
/// secrets that identify its controller and its agents. Persisted in the relay
/// config so tenants survive restarts.
#[derive(Clone, Serialize, Deserialize)]
struct TenantRecord {
	id: String,
	name: String,
	owner_secret: String,
	agent_secret: String,
}

impl TenantRecord {
	/// Mint a fresh tenant with random id + secrets.
	fn generate(name: String) -> Self {
		Self {
			id: random_alnum(TENANT_ID_LEN),
			name,
			owner_secret: random_alnum(SECRET_LEN),
			agent_secret: random_alnum(SECRET_LEN),
		}
	}
}

#[derive(Serialize, Deserialize)]
struct ServerConfig {
	/// Gates the admin/provisioning channel. Held by the relay operator; whoever
	/// has it can mint, list and revoke tenants.
	admin_secret: String,
	/// When true, any client may provision a tenant without the admin secret (it can
	/// still only provision — listing and revoking always require the admin secret).
	#[serde(default)]
	open_registration: bool,
	/// The provisioned tenants, each with its own owner/agent secrets and isolated
	/// routing. Empty on a fresh relay until the first tenant is provisioned.
	#[serde(default)]
	tenants: Vec<TenantRecord>,
	cert_der: String,
	key_der: String,
	/// Optional browser portal (serves the embedded SPA so a new machine can pair
	/// from a browser). Absent on existing relays — the portal is opt-in; the relay
	/// runs QUIC-only without it.
	#[serde(default)]
	portal: Option<portal::PortalConfig>,
}

impl ServerConfig {
	fn generate() -> Self {
		let (cert_der, key_der) = tls::self_signed();
		Self {
			admin_secret: random_alnum(SECRET_LEN),
			open_registration: false,
			tenants: Vec::new(),
			cert_der: B64.encode(cert_der),
			key_der: B64.encode(key_der),
			portal: None,
		}
	}

	fn cert_key(&self) -> Result<(Vec<u8>, Vec<u8>)> {
		Ok((B64.decode(&self.cert_der)?, B64.decode(&self.key_der)?))
	}

	/// Refuse to operate with a blank secret. An empty `admin_secret` — or a tenant
	/// with an empty owner/agent secret — would make `ct_eq("", "")` true, i.e.
	/// authenticate any peer presenting an empty secret, a fail-open we reject
	/// outright. A freshly generated config is always valid; this only catches a
	/// hand-edited/truncated one.
	fn validate(&self) -> Result<()> {
		if self.admin_secret.trim().is_empty() {
			anyhow::bail!("has a blank admin secret — delete it to regenerate, or restore the secret");
		}
		for t in &self.tenants {
			if t.owner_secret.trim().is_empty() || t.agent_secret.trim().is_empty() {
				anyhow::bail!(
					"tenant {} ({}) has a blank owner/agent secret — remove it from the config and re-provision",
					t.id,
					t.name
				);
			}
		}
		Ok(())
	}
}

/// Env var overriding the QUIC listen address, so a relay can be reconfigured from
/// docker-compose without a custom command. Falls back to [`default_listen`].
const LISTEN_ENV: &str = "LIBRETETHER_LISTEN";

/// The default QUIC listen address: dual-stack on the default port. `[::]` accepts
/// IPv6 and — on a normal dual-stack host — IPv4-mapped clients too. The listen
/// address is a deployment knob, not persisted config, so the image is dual-stack
/// out of the box; override it with `--listen` or [`LISTEN_ENV`].
fn default_listen() -> String {
	format!("[::]:{DEFAULT_PORT}")
}

/// Resolve the QUIC listen address: the `--listen` flag, else [`LISTEN_ENV`], else
/// the dual-stack [`default_listen`].
fn resolve_listen(flag: Option<String>) -> String {
	resolve_listen_inner(flag, std::env::var(LISTEN_ENV).ok())
}

/// [`resolve_listen`] split from its env read so the precedence is testable.
fn resolve_listen_inner(flag: Option<String>, env: Option<String>) -> String {
	[flag, env]
		.into_iter()
		.flatten()
		.map(|s| s.trim().to_string())
		.find(|s| !s.is_empty())
		.unwrap_or_else(default_listen)
}

fn config_path(arg: Option<PathBuf>) -> PathBuf {
	arg.unwrap_or_else(|| {
		dirs::config_dir()
			.unwrap_or_else(|| PathBuf::from("."))
			.join("libretether-relay")
			.join("config.json")
	})
}

/// Parse and validate a config from its on-disk text.
fn parse_config(raw: &str, path: &Path) -> Result<ServerConfig> {
	let cfg: ServerConfig = serde_json::from_str(raw).context("parsing server config")?;
	cfg.validate()
		.with_context(|| format!("config at {}", path.display()))?;
	Ok(cfg)
}

/// Read an existing config, never creating one. `info` uses this: generating a
/// fresh config here would print freshly-minted secrets that don't match the
/// running relay — e.g. when `--config` is omitted and a default path the relay
/// never used is consulted — which is more dangerous than a clean failure. Fail
/// closed with an actionable error instead.
fn load(path: &PathBuf) -> Result<ServerConfig> {
	match std::fs::read_to_string(path) {
		Ok(raw) => parse_config(&raw, path),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => anyhow::bail!(
			"no relay config at {} — run the relay (`libretether-relay run`) first to \
			 generate one, or pass --config <path> to point at an existing config",
			path.display()
		),
		Err(e) => Err(anyhow::Error::new(e).context(format!("reading relay config at {}", path.display()))),
	}
}

/// Read the config, generating and persisting a fresh one on first run. `run`
/// uses this; `info` must not (see [`load`]).
fn load_or_create(path: &PathBuf) -> Result<ServerConfig> {
	match std::fs::read_to_string(path) {
		Ok(raw) => parse_config(&raw, path),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			let cfg = ServerConfig::generate();
			// The config holds the owner/agent secrets and TLS key — write it
			// owner-only so other local users on the relay host can't read them.
			secret::write_str(path, &serde_json::to_string_pretty(&cfg)?)?;
			Ok(cfg)
		}
		Err(e) => Err(e.into()),
	}
}

// ---------------------------------------------------------------- relay state

/// Hands out a unique generation to each controller session so a stale session's
/// teardown can't clear a newer session's event sender (see `serve_controller`).
static CONTROLLER_GEN: AtomicU64 = AtomicU64::new(0);

/// The single connected controller's session, tagged with its generation. We
/// keep its connection so a newer controller can tear the old one down.
struct ControllerSession {
	generation: u64,
	events: UnboundedSender<RelayEvent>,
	conn: quinn::Connection,
}

/// A controller's parked pairing slot: the relay-side halves of the stream the
/// controller opened with [`RelayRequest::OpenPairing`], held until a `Pairing`
/// peer joins with the matching nameplate (then piped together) or the slot
/// expires. Dropping it resets both streams, which the controller surfaces as
/// "pairing expired".
struct Pending {
	send: SendStream,
	recv: RecvStream,
	created: Instant,
}

/// A registered agent: its routing connection plus a channel to push relay→agent
/// signals (e.g. a hole-punch request) on its otherwise-idle hello stream.
struct AgentHandle {
	conn: quinn::Connection,
	signals: UnboundedSender<RelaySignal>,
}

/// One tenant's live routing state: the credentials that identify it, plus the
/// controller slot, agent map and log ring that are isolated from every other
/// tenant. Shared as an `Arc` between the tenant's connection tasks. Pairing slots
/// are *not* here — they're global on [`Relay`], because a pairing pipes the
/// controller's own parked stream (which already belongs to the right tenant).
struct Tenant {
	id: String,
	name: String,
	owner_secret: String,
	agent_secret: String,
	agents: Mutex<HashMap<String, AgentHandle>>,
	controller: Mutex<Option<ControllerSession>>,
	/// This tenant's own log ring; only its controller can fetch it.
	log: logbook::Log,
}

impl Tenant {
	fn from_record(rec: &TenantRecord) -> Arc<Self> {
		Arc::new(Self {
			id: rec.id.clone(),
			name: rec.name.clone(),
			owner_secret: rec.owner_secret.clone(),
			agent_secret: rec.agent_secret.clone(),
			agents: Mutex::new(HashMap::new()),
			controller: Mutex::new(None),
			log: logbook::Log::new(tenant_tag(&rec.id, &rec.name)),
		})
	}

	/// The durable record for this tenant (persisted to the config on mutation).
	fn record(&self) -> TenantRecord {
		TenantRecord {
			id: self.id.clone(),
			name: self.name.clone(),
			owner_secret: self.owner_secret.clone(),
			agent_secret: self.agent_secret.clone(),
		}
	}

	/// This tenant's public status (no secrets) for an admin `List`.
	fn info(&self) -> TenantInfo {
		TenantInfo {
			tenant_id: self.id.clone(),
			name: self.name.clone(),
			controller_online: self.controller.lock().unwrap().is_some(),
			agents_online: self.agents.lock().unwrap().len(),
		}
	}

	fn agent(&self, public_key: &str) -> Option<quinn::Connection> {
		self.agents.lock().unwrap().get(public_key).map(|h| h.conn.clone())
	}

	/// The agent's reflexive address as the relay observed it (the source address of
	/// its QUIC connection) — the "STUN" half of brokering a peer-to-peer punch.
	fn agent_addr(&self, public_key: &str) -> Option<SocketAddr> {
		self.agents
			.lock()
			.unwrap()
			.get(public_key)
			.map(|h| h.conn.remote_address())
	}

	/// Push a signal to a registered agent over its hello-stream channel. Returns
	/// false if the agent isn't registered (or its signal task has ended).
	fn signal_agent(&self, public_key: &str, signal: RelaySignal) -> bool {
		match self.agents.lock().unwrap().get(public_key) {
			Some(h) => h.signals.send(signal).is_ok(),
			None => false,
		}
	}

	/// Notify this tenant's controller (if connected) of a presence event.
	fn notify(&self, event: RelayEvent) {
		if let Some(session) = self.controller.lock().unwrap().as_ref() {
			let _ = session.events.send(event);
		}
	}
}

/// A short stderr/label tag for a tenant: its name plus a key8-style id prefix, so
/// `docker logs` attributes a per-tenant line and two same-named tenants stay
/// distinguishable.
fn tenant_tag(id: &str, name: &str) -> String {
	format!("{name}·{}", id.chars().take(6).collect::<String>())
}

/// Everything needed to rewrite the on-disk config when the tenant set changes:
/// the config path and the config fields the relay doesn't otherwise mutate at
/// runtime. Held behind an `Arc` on [`Relay`].
struct Persist {
	path: PathBuf,
	admin_secret: String,
	open_registration: bool,
	cert_der: String,
	key_der: String,
	portal: Option<portal::PortalConfig>,
}

impl Persist {
	/// Rewrite the config with `tenants` as the current set. Owner-only perms, like
	/// the initial write — the file holds every tenant's secrets and the TLS key.
	fn save(&self, tenants: Vec<TenantRecord>) -> Result<()> {
		let cfg = ServerConfig {
			admin_secret: self.admin_secret.clone(),
			open_registration: self.open_registration,
			tenants,
			cert_der: self.cert_der.clone(),
			key_der: self.key_der.clone(),
			portal: self.portal.clone(),
		};
		secret::write_str(&self.path, &serde_json::to_string_pretty(&cfg)?)?;
		Ok(())
	}
}

#[derive(Clone)]
struct Relay {
	/// Tenants keyed by tenant id. The credentials in each are used to resolve which
	/// tenant a connecting controller/agent belongs to (see `authenticate`).
	tenants: Arc<Mutex<HashMap<String, Arc<Tenant>>>>,
	/// Open pairing slots keyed by nameplate (see [`crate::Pending`]). Global, not
	/// per-tenant: a pairing pipes the controller's own parked stream, so the tenant
	/// association is already carried by that stream. The relay only matches by
	/// nameplate and pipes the two streams; it never sees the PAKE password or the
	/// enrollment bundle that flow over the pipe.
	pairings: Arc<Mutex<HashMap<String, Pending>>>,
	/// Per-source-IP fixed-window connection counters for rate limiting.
	limiter: Arc<Mutex<HashMap<IpAddr, (u32, Instant)>>>,
	/// The relay admin secret, gating the provisioning channel.
	admin_secret: Arc<String>,
	/// Whether any client may provision a tenant without the admin secret.
	open_registration: bool,
	/// Config-persistence handle, used when the tenant set changes.
	persist: Arc<Persist>,
}

impl Relay {
	/// Build the runtime relay from a loaded config and the path to persist back to.
	fn new(cfg: &ServerConfig, config_path: PathBuf) -> Self {
		let tenants = cfg
			.tenants
			.iter()
			.map(|rec| (rec.id.clone(), Tenant::from_record(rec)))
			.collect();
		Self {
			tenants: Arc::new(Mutex::new(tenants)),
			pairings: Arc::new(Mutex::new(HashMap::new())),
			limiter: Arc::new(Mutex::new(HashMap::new())),
			admin_secret: Arc::new(cfg.admin_secret.clone()),
			open_registration: cfg.open_registration,
			persist: Arc::new(Persist {
				path: config_path,
				admin_secret: cfg.admin_secret.clone(),
				open_registration: cfg.open_registration,
				cert_der: cfg.cert_der.clone(),
				key_der: cfg.key_der.clone(),
				portal: cfg.portal.clone(),
			}),
		}
	}

	/// Resolve the tenant whose owner secret matches (constant-time per tenant), or
	/// `None` if no tenant claims it. Iterating and `ct_eq`-ing each keeps the
	/// byte-level timing resistance of the single-tenant path; a match only reveals
	/// *which* tenant matched, which the connecting controller already knows.
	fn tenant_by_owner(&self, secret: &str) -> Option<Arc<Tenant>> {
		self.tenants
			.lock()
			.unwrap()
			.values()
			.find(|t| crypto::ct_eq(secret, &t.owner_secret))
			.cloned()
	}

	/// Resolve the tenant whose agent secret matches (see [`Relay::tenant_by_owner`]).
	fn tenant_by_agent(&self, secret: &str) -> Option<Arc<Tenant>> {
		self.tenants
			.lock()
			.unwrap()
			.values()
			.find(|t| crypto::ct_eq(secret, &t.agent_secret))
			.cloned()
	}

	/// Whether the presented secret is the admin secret (constant-time).
	fn is_admin_secret(&self, secret: &str) -> bool {
		crypto::ct_eq(secret, &self.admin_secret)
	}

	/// Public status of every tenant, for an admin `List` (never includes secrets).
	fn tenant_infos(&self) -> Vec<TenantInfo> {
		self.tenants.lock().unwrap().values().map(|t| t.info()).collect()
	}

	/// Mint a new tenant, register it live, and persist the updated set. Returns the
	/// tenant's freshly-generated credentials, or an error string (blank name / at
	/// capacity / persistence failure — with the in-memory insert rolled back so
	/// memory and disk stay consistent).
	fn provision(&self, name: String) -> std::result::Result<TenantCredentials, String> {
		let name = name.trim();
		if name.is_empty() {
			return Err("tenant name must not be blank".into());
		}
		let mut rec = TenantRecord::generate(name.to_string());
		// Hold the tenants lock across the (small, sync) config write so concurrent
		// provisions/revokes can't interleave into a lost update on disk.
		let mut tenants = self.tenants.lock().unwrap();
		if tenants.len() >= MAX_TENANTS {
			return Err(format!("relay at tenant capacity ({MAX_TENANTS})"));
		}
		// A random 12-char id practically never collides, but never overwrite a live
		// tenant if it somehow does.
		while tenants.contains_key(&rec.id) {
			rec.id = random_alnum(TENANT_ID_LEN);
		}
		let creds = TenantCredentials {
			tenant_id: rec.id.clone(),
			name: rec.name.clone(),
			owner_secret: rec.owner_secret.clone(),
			agent_secret: rec.agent_secret.clone(),
		};
		tenants.insert(rec.id.clone(), Tenant::from_record(&rec));
		let snapshot = tenants.values().map(|t| t.record()).collect();
		if let Err(e) = self.persist.save(snapshot) {
			tenants.remove(&rec.id);
			return Err(format!("persisting tenant: {e:#}"));
		}
		Ok(creds)
	}

	/// Remove a tenant, persist the change, and disconnect its controller + agents so
	/// the revoke takes effect immediately. `Ok(false)` if no tenant had that id.
	fn revoke(&self, id: &str) -> std::result::Result<bool, String> {
		let tenant = {
			let mut tenants = self.tenants.lock().unwrap();
			let Some(tenant) = tenants.remove(id) else {
				return Ok(false);
			};
			let snapshot = tenants.values().map(|t| t.record()).collect();
			if let Err(e) = self.persist.save(snapshot) {
				// Roll back so a failed write can't drop the tenant from disk on the next
				// restart while we've already removed it from memory.
				tenants.insert(id.to_string(), tenant);
				return Err(format!("persisting revoke: {e:#}"));
			}
			tenant
		};
		if let Some(session) = tenant.controller.lock().unwrap().take() {
			session.conn.close(0u32.into(), b"tenant revoked");
		}
		for (_, handle) in tenant.agents.lock().unwrap().drain() {
			handle.conn.close(0u32.into(), b"tenant revoked");
		}
		Ok(true)
	}

	/// Fixed-window per-IP rate check: returns false once a source exceeds
	/// [`RATE_LIMIT_PER_WINDOW`] new connections within [`RATE_WINDOW`].
	fn allow(&self, ip: IpAddr) -> bool {
		self.allow_at(ip, Instant::now())
	}

	/// [`Relay::allow`] with an injectable clock so the window rollover and stale
	/// eviction are deterministically testable.
	fn allow_at(&self, ip: IpAddr, now: Instant) -> bool {
		let mut map = self.limiter.lock().unwrap();
		// Opportunistically drop stale entries so the map can't grow unbounded.
		if map.len() > 10_000 {
			map.retain(|_, (_, t)| now.duration_since(*t) < RATE_WINDOW);
		}
		let entry = map.entry(ip).or_insert((0, now));
		if now.duration_since(entry.1) >= RATE_WINDOW {
			*entry = (0, now);
		}
		entry.0 += 1;
		entry.0 <= RATE_LIMIT_PER_WINDOW
	}

	/// Park a controller's pairing slot under `nameplate`. Returns `false` (caller
	/// should reset the controller's stream) only if the map is genuinely full of
	/// live slots. Last-writer-wins on a nameplate collision — the controller owns its
	/// own random nameplates, so a clash just means it reused one.
	fn register_pairing(&self, nameplate: String, pending: Pending) -> bool {
		self.register_pairing_at(nameplate, pending, Instant::now())
	}

	/// [`Relay::register_pairing`] with an injectable clock for deterministic tests.
	fn register_pairing_at(&self, nameplate: String, pending: Pending, now: Instant) -> bool {
		let mut map = self.pairings.lock().unwrap();
		// Reclaim expired slots before enforcing the cap so a burst of abandoned slots
		// can't wedge pairing for everyone.
		map.retain(|_, p| now.duration_since(p.created) < PAIRING_TTL);
		if map.len() >= MAX_PENDING_PAIRINGS && !map.contains_key(&nameplate) {
			return false;
		}
		map.insert(nameplate, pending);
		true
	}

	/// Remove and return the slot for `nameplate`, but only if present and unexpired
	/// (single-use: a joined or expired slot is gone). An expired slot is dropped here,
	/// resetting the controller's parked stream.
	fn take_pairing(&self, nameplate: &str) -> Option<Pending> {
		self.take_pairing_at(nameplate, Instant::now())
	}

	/// [`Relay::take_pairing`] with an injectable clock for deterministic tests.
	fn take_pairing_at(&self, nameplate: &str, now: Instant) -> Option<Pending> {
		let pending = self.pairings.lock().unwrap().remove(nameplate)?;
		(now.duration_since(pending.created) < PAIRING_TTL).then_some(pending)
	}

	/// Drop every expired pairing slot. Run on a timer so a slot the operator opened
	/// and abandoned (closed the dialog) doesn't sit parked until the process exits.
	fn sweep_pairings_at(&self, now: Instant) {
		self.pairings
			.lock()
			.unwrap()
			.retain(|_, p| now.duration_since(p.created) < PAIRING_TTL);
	}
}

#[derive(Parser)]
#[command(name = "libretether-relay", version, about = "LibreTether relay server")]
struct Cli {
	/// Path to the server config file.
	#[arg(long, global = true)]
	config: Option<PathBuf>,

	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand)]
enum Command {
	/// Run the relay.
	Run {
		/// Override the QUIC listen address (default `[::]:47600`, dual-stack; also
		/// settable via the `LIBRETETHER_LISTEN` env var).
		#[arg(long)]
		listen: Option<String>,
	},
	/// Print the listen address, the admin secret, and a tenant summary.
	Info,
	/// Manage tenants directly in the config. Intended for a stopped relay or for
	/// scripting; a *running* relay only picks up on-disk tenant changes at its next
	/// restart. For live changes, provision from the app's admin channel instead.
	#[command(subcommand)]
	Tenant(TenantCmd),
}

#[derive(Subcommand)]
enum TenantCmd {
	/// Provision a new tenant and print its owner/agent secrets.
	Add {
		/// A human label for the tenant (e.g. a team or person).
		#[arg(long)]
		name: String,
	},
	/// List provisioned tenants (ids + names, no secrets).
	List,
	/// Remove a tenant by id.
	Rm {
		/// The tenant id (from `tenant list`).
		id: String,
	},
}

#[tokio::main]
async fn main() -> Result<()> {
	let cli = Cli::parse();
	let path = config_path(cli.config.clone());

	match cli.command {
		Command::Info => {
			// Read-only: never generate a config, so we can't print secrets that
			// wouldn't match the running relay — see `load`. The listen address isn't in
			// the config, so report the one `run` would resolve from the env/default.
			print_credentials(&load(&path)?, &resolve_listen(None));
			Ok(())
		}
		Command::Tenant(cmd) => run_tenant_cmd(&path, cmd),
		Command::Run { listen } => {
			let cfg = load_or_create(&path)?;
			let listen_addr = resolve_listen(listen);
			// The data dir (where the config lives) is where the portal's ACME cache goes,
			// so an auto-issued cert persists in the same volume across restarts.
			let data_dir = path
				.parent()
				.map(Path::to_path_buf)
				.unwrap_or_else(|| PathBuf::from("."));
			run(cfg, data_dir, listen_addr, path).await
		}
	}
}

fn print_credentials(cfg: &ServerConfig, listen: &str) {
	println!("listen:            {listen}");
	println!("admin secret:      {}", cfg.admin_secret);
	println!(
		"open registration: {}",
		if cfg.open_registration { "on" } else { "off" }
	);
	println!("tenants:           {}", cfg.tenants.len());
	println!();
	println!("Provision a tenant from the app (New controller → Relay → Provision) with the");
	println!("admin secret, or run `libretether-relay tenant add --name <name>`. Each tenant");
	println!("gets its own owner secret (its controller) and agent secret (its deploy scripts).");
}

/// Handle the on-disk `tenant` subcommands. These mutate `config.json`; a running
/// relay only sees the change on its next restart (documented on the subcommand).
fn run_tenant_cmd(path: &PathBuf, cmd: TenantCmd) -> Result<()> {
	match cmd {
		TenantCmd::List => {
			let cfg = load(path)?;
			if cfg.tenants.is_empty() {
				println!("no tenants provisioned");
			} else {
				for t in &cfg.tenants {
					println!("{}  {}", t.id, t.name);
				}
			}
			Ok(())
		}
		TenantCmd::Add { name } => {
			let name = name.trim();
			if name.is_empty() {
				anyhow::bail!("tenant name must not be blank");
			}
			// `load_or_create` so `tenant add` also bootstraps a brand-new relay's config.
			let mut cfg = load_or_create(path)?;
			if cfg.tenants.len() >= MAX_TENANTS {
				anyhow::bail!("relay at tenant capacity ({MAX_TENANTS})");
			}
			let rec = TenantRecord::generate(name.to_string());
			let (id, owner, agent) = (rec.id.clone(), rec.owner_secret.clone(), rec.agent_secret.clone());
			cfg.tenants.push(rec);
			secret::write_str(path, &serde_json::to_string_pretty(&cfg)?)?;
			println!("tenant id:    {id}");
			println!("name:         {name}");
			println!("owner secret: {owner}");
			println!("agent secret: {agent}");
			println!();
			println!("A running relay picks this up on its next restart.");
			Ok(())
		}
		TenantCmd::Rm { id } => {
			let mut cfg = load(path)?;
			let before = cfg.tenants.len();
			cfg.tenants.retain(|t| t.id != id);
			if cfg.tenants.len() == before {
				anyhow::bail!("no tenant with id {id}");
			}
			secret::write_str(path, &serde_json::to_string_pretty(&cfg)?)?;
			println!("removed tenant {id}");
			Ok(())
		}
	}
}

async fn run(cfg: ServerConfig, data_dir: PathBuf, listen_addr: String, config_path: PathBuf) -> Result<()> {
	let (cert, key) = cfg.cert_key()?;
	let addr: SocketAddr = listen_addr.parse().context("invalid listen address")?;
	// Dual-stack when the listen address is `[::]` (the default), so IPv4 peers reach
	// the relay too even under Windows/BSD where `IPV6_V6ONLY` defaults on. See
	// `tls::server_endpoint`.
	let endpoint = tls::server_endpoint(cert, key, addr).context("bind relay QUIC listener")?;
	logbook::info(&format!("relay listening on udp/{addr}"));
	logbook::info(&format!(
		"{} tenant(s) loaded; open registration {}",
		cfg.tenants.len(),
		if cfg.open_registration { "on" } else { "off" }
	));
	// Don't echo the secrets on every `run` — they'd persist in the journal /
	// `docker logs` for the life of the deployment. `libretether-relay info` prints
	// the admin secret on demand.
	logbook::info("run `libretether-relay info` to print the admin secret");

	// Build the runtime relay (seeding its tenants from the config) before moving
	// `cfg.portal` into the portal task — `Relay::new` clones what it needs to persist.
	let relay = Relay::new(&cfg, config_path);

	// Bring up the browser portal (serves the embedded pairing SPA) when configured —
	// via the config's `portal` block or env vars (so docker-compose alone can enable
	// it). It runs in its own tasks; a bind failure disables it rather than taking down
	// the relay's QUIC service.
	if let Some(portal_cfg) = portal::resolve_config(cfg.portal) {
		tokio::spawn(async move {
			if let Err(e) = portal::run(portal_cfg, data_dir).await {
				logbook::warn(&format!("portal disabled: {e:#}"));
			}
		});
	}

	let conn_limit = Arc::new(Semaphore::new(MAX_CONNECTIONS));
	// Agents draw from a smaller pool so they can never consume the controller's
	// reserved headroom (see CONTROLLER_RESERVED). The role isn't known until after
	// auth, so this is acquired inside `handle` once the peer proves it's an agent.
	let agent_limit = Arc::new(Semaphore::new(MAX_CONNECTIONS - CONTROLLER_RESERVED));

	// Drop abandoned pairing slots on a timer so they don't park until shutdown.
	let mut pairing_sweep = tokio::time::interval(PAIRING_SWEEP_INTERVAL);

	loop {
		tokio::select! {
			_ = pairing_sweep.tick() => {
				relay.sweep_pairings_at(Instant::now());
				continue;
			}
			incoming = endpoint.accept() => {
				let Some(incoming) = incoming else { break };
				// Shed obvious abuse before doing any handshake work: rate-limit per
				// source IP, then cap total concurrent connections.
				if !relay.allow(incoming.remote_address().ip()) {
					incoming.refuse();
					continue;
				}
				let Ok(permit) = conn_limit.clone().try_acquire_owned() else {
					incoming.refuse();
					continue;
				};
				let relay = relay.clone();
				let agent_limit = agent_limit.clone();
				tokio::spawn(async move {
					let _permit = permit; // released when the connection task ends
					if let Err(e) = handle(relay, incoming, &agent_limit).await {
						logbook::warn(&format!("connection error: {e}"));
					}
				});
			}
			_ = shutdown_signal() => {
				logbook::info("shutting down");
				break;
			}
		}
	}
	// Tell peers we're going away so they reconnect promptly instead of waiting
	// out the idle timeout, then exit cleanly (so `docker stop` is graceful).
	endpoint.close(0u32.into(), b"relay shutting down");
	Ok(())
}

async fn handle(relay: Relay, incoming: quinn::Incoming, agent_limit: &Arc<Semaphore>) -> Result<()> {
	logbook::debug(&format!("connection received from {}", incoming.remote_address()));
	// Bound the whole pre-serve phase — the QUIC/TLS handshake AND the app-level
	// auth — under one timeout. The connection permit is acquired at accept (before
	// either runs), so a peer that completes the UDP path then stalls at *either*
	// stage must not be able to hold that permit for longer than HANDSHAKE_TIMEOUT.
	let pre = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
		let conn = incoming.accept()?.await?;
		let authed = authenticate(&relay, &conn).await?;
		Ok::<_, anyhow::Error>((conn, authed))
	})
	.await;
	let (conn, authed) = match pre {
		Ok(Ok((conn, Some(authed)))) => (conn, authed),
		Ok(Ok((_, None))) => return Ok(()), // cleanly rejected (bad secret / proof)
		Ok(Err(e)) => return Err(e),        // QUIC or I/O error during the handshake
		Err(_) => return Ok(()),            // QUIC handshake or auth timed out
	};

	match authed {
		Authed::Controller { tenant, send } => serve_controller(relay, tenant, conn, send).await,
		Authed::Admin { full, send } => serve_admin(relay, full, conn, send).await,
		Authed::Pairing { nameplate, send, recv } => {
			// A pairing join holds no long-lived resource and isn't trusted: match it to
			// a controller's open slot by nameplate and pipe, or reset it.
			serve_pairing_join(relay, nameplate, send, recv).await
		}
		Authed::Agent {
			tenant,
			public_key,
			send,
		} => {
			// Reserve controller headroom: agents acquire from the smaller agent pool
			// so an agent-secret holder opening connections in bulk (even under freshly
			// minted keys, which the key-ownership proof can't prevent) can't drain the
			// global pool and lock the controllers out. Held for the connection's life.
			let Ok(permit) = agent_limit.clone().try_acquire_owned() else {
				logbook::warn("agent connection refused: at agent capacity");
				return Ok(());
			};
			serve_agent(tenant, conn, public_key, send, permit).await
		}
	}
}

/// A successfully-authenticated (or, for pairing, accepted-without-auth) relay peer,
/// resolved to its tenant where applicable. Carries the hello stream's send half
/// (the controller writes presence events on it; the agent reuses it for signals).
enum Authed {
	Controller {
		tenant: Arc<Tenant>,
		send: SendStream,
	},
	Agent {
		tenant: Arc<Tenant>,
		public_key: String,
		send: SendStream,
	},
	/// An admin/provisioning client. `full` is true when it presented the admin
	/// secret (may list/revoke); false when admitted via open registration
	/// (provision-only).
	Admin {
		full: bool,
		send: SendStream,
	},
	/// A pairing join: carries both halves of its stream (to pipe) and its nameplate.
	Pairing {
		nameplate: String,
		send: SendStream,
		recv: RecvStream,
	},
}

/// Reject a peer cleanly: write a negative ack (best-effort) and let the caller
/// return `Ok(None)`.
async fn reject(send: &mut SendStream, reason: &str) {
	let _ = write_frame(
		send,
		&RelayAck {
			accepted: false,
			reason: Some(reason.into()),
		},
	)
	.await;
}

/// Which tenant/role a hello resolves to, before the key-ownership challenge.
enum Who {
	Controller(Arc<Tenant>),
	Agent(Arc<Tenant>),
	Admin { full: bool },
}

/// Resolve the peer's tenant/role from its secret, prove it holds the private key
/// for the public key it presented, and return the [`Authed`]. `None` on a clean
/// rejection (unknown secret, or a failed key-ownership proof).
///
/// The tenant a controller/agent belongs to is identified by the secret it presents
/// — the relay looks up which tenant's owner/agent secret matches. `Admin` matches
/// the relay admin secret (or, under open registration, is admitted provision-only).
///
/// The `Pairing` role is the exception: it carries no secret and isn't trusted by
/// the relay at all — the relay only matches it to a controller's open slot by
/// nameplate and pipes the two together, so the PAKE over that pipe is the actual
/// authentication. It's accepted here without a secret or key-ownership proof.
async fn authenticate(relay: &Relay, conn: &quinn::Connection) -> Result<Option<Authed>> {
	let (mut send, mut recv) = conn.accept_bi().await.context("accept hello stream")?;
	let hello: RelayHello = read_frame_capped(&mut recv, MAX_CONTROL_FRAME).await?;

	// Pairing: no secret, no proof — see the doc comment. Require a non-empty
	// nameplate; a malformed pairing hello is a clean rejection.
	if let RelayRole::Pairing = hello.role {
		let Some(nameplate) = hello.nameplate.filter(|n| !n.trim().is_empty()) else {
			logbook::warn("rejected pairing join: missing nameplate");
			return Ok(None);
		};
		logbook::debug("pairing join received");
		return Ok(Some(Authed::Pairing { nameplate, send, recv }));
	}

	// Resolve who this is from the presented secret (constant-time). A controller/
	// agent that matches no tenant, or an admin without the secret when open
	// registration is off, is a clean "bad secret" rejection.
	let who = match hello.role {
		RelayRole::Controller => match relay.tenant_by_owner(&hello.secret) {
			Some(tenant) => Who::Controller(tenant),
			None => {
				logbook::warn("rejected controller: no tenant for that owner secret");
				reject(&mut send, "bad secret").await;
				return Ok(None);
			}
		},
		RelayRole::Agent => match relay.tenant_by_agent(&hello.secret) {
			Some(tenant) => Who::Agent(tenant),
			None => {
				logbook::warn("rejected agent: no tenant for that agent secret");
				reject(&mut send, "bad secret").await;
				return Ok(None);
			}
		},
		RelayRole::Admin => {
			let full = relay.is_admin_secret(&hello.secret);
			if full || relay.open_registration {
				Who::Admin { full }
			} else {
				logbook::warn("rejected admin: bad secret and open registration is off");
				reject(&mut send, "bad secret").await;
				return Ok(None);
			}
		}
		RelayRole::Pairing => unreachable!("handled above"),
	};

	// Prove possession of the presented Ed25519 key before trusting it — in
	// particular before registering an agent under it for routing, so a holder of a
	// (shared) agent secret can't hijack another agent's key. Harmless but uniform
	// for the admin (whose key is ephemeral).
	let nonce = crypto::random_nonce_b64();
	write_frame(&mut send, &RelayChallenge { nonce: nonce.clone() }).await?;
	let proof: RelayProof = read_frame_capped(&mut recv, MAX_CONTROL_FRAME).await?;
	if !crypto::verify_b64(&hello.public_key, nonce.as_bytes(), &proof.signature) {
		logbook::warn("rejected relay peer: key ownership proof failed");
		reject(&mut send, "key ownership proof failed").await;
		return Ok(None);
	}

	write_frame(
		&mut send,
		&RelayAck {
			accepted: true,
			reason: None,
		},
	)
	.await?;

	// Route on the canonical key bytes, not the raw wire string: two base64
	// encodings of the same key must resolve to one routing entry. `verify_b64`
	// already proved it's a real 32-byte key, so canonicalization can't fail here.
	let public_key = crypto::canonical_pubkey(&hello.public_key).unwrap_or(hello.public_key);

	Ok(Some(match who {
		Who::Controller(tenant) => {
			tenant
				.log
				.info(&format!("controller connected ({}…)", key8(&public_key)));
			Authed::Controller { tenant, send }
		}
		Who::Agent(tenant) => {
			tenant.log.info(&format!("agent connected ({}…)", key8(&public_key)));
			Authed::Agent {
				tenant,
				public_key,
				send,
			}
		}
		Who::Admin { full } => {
			logbook::info(&format!(
				"admin connected ({})",
				if full { "full" } else { "registration-only" }
			));
			Authed::Admin { full, send }
		}
	}))
}

/// Serve an admin/provisioning session: each stream the client opens carries one
/// [`AdminRequest`] we answer with an [`AdminResponse`]. `full` gates `List`/`Revoke`
/// (which require the admin secret); a registration-only client may only `Provision`.
async fn serve_admin(relay: Relay, full: bool, conn: quinn::Connection, _hello_send: SendStream) -> Result<()> {
	loop {
		let (mut send, mut recv) = match conn.accept_bi().await {
			Ok(pair) => pair,
			Err(_) => break,
		};
		let relay = relay.clone();
		tokio::spawn(async move {
			let Ok(req) = read_frame_capped::<_, AdminRequest>(&mut recv, MAX_CONTROL_FRAME).await else {
				return;
			};
			let resp = handle_admin(&relay, full, req);
			let _ = write_frame(&mut send, &resp).await;
			let _ = send.finish();
		});
	}
	logbook::info("admin session ended");
	Ok(())
}

/// Apply one [`AdminRequest`] against the tenant registry (persisting mutations) and
/// produce the reply. `full` gates the management operations.
fn handle_admin(relay: &Relay, full: bool, req: AdminRequest) -> AdminResponse {
	match req {
		AdminRequest::Provision { name } => match relay.provision(name) {
			Ok(creds) => {
				logbook::info(&format!("provisioned tenant {} ({})", creds.tenant_id, creds.name));
				AdminResponse::Provisioned(creds)
			}
			Err(message) => {
				logbook::warn(&format!("provision refused: {message}"));
				AdminResponse::Error { message }
			}
		},
		AdminRequest::List => {
			if !full {
				return AdminResponse::Error {
					message: "listing tenants requires the admin secret".into(),
				};
			}
			AdminResponse::Tenants {
				tenants: relay.tenant_infos(),
			}
		}
		AdminRequest::Revoke { tenant_id } => {
			if !full {
				return AdminResponse::Error {
					message: "revoking a tenant requires the admin secret".into(),
				};
			}
			match relay.revoke(&tenant_id) {
				Ok(existed) => {
					if existed {
						logbook::info(&format!("revoked tenant {tenant_id}"));
					}
					AdminResponse::Revoked { tenant_id, existed }
				}
				Err(message) => AdminResponse::Error { message },
			}
		}
	}
}

/// The controller pushes presence events out on `events`, and opens one routed
/// bi stream per request which we pipe to the addressed agent — all scoped to its
/// `tenant`.
async fn serve_controller(
	relay: Relay,
	tenant: Arc<Tenant>,
	conn: quinn::Connection,
	mut events: SendStream,
) -> Result<()> {
	let generation = CONTROLLER_GEN.fetch_add(1, Ordering::Relaxed);
	tenant
		.log
		.info(&format!("controller session started (gen {generation})"));
	let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RelayEvent>();

	// Install our sender (and connection) and snapshot existing agents under one
	// lock hold: doing it atomically means an agent that connects mid-attach is
	// delivered via the sender rather than missed (presence race). Any previous
	// controller is displaced and its connection closed, so a second owner can't
	// leave a zombie routing loop running.
	let previous = {
		let agents = tenant.agents.lock().unwrap();
		let mut slot = tenant.controller.lock().unwrap();
		let previous = slot.replace(ControllerSession {
			generation,
			events: tx.clone(),
			conn: conn.clone(),
		});
		for key in agents.keys() {
			let _ = tx.send(RelayEvent::AgentOnline {
				public_key: key.clone(),
			});
		}
		previous
	};
	if let Some(prev) = previous {
		tenant.log.info(&format!(
			"displacing previous controller (gen {}) for the new session",
			prev.generation
		));
		prev.conn.close(0u32.into(), b"replaced by a newer controller");
	}

	// Forward presence events to the controller.
	let events_task = tokio::spawn(async move {
		while let Some(event) = rx.recv().await {
			if write_frame(&mut events, &event).await.is_err() {
				break;
			}
		}
	});

	// Emit a periodic heartbeat so the controller can tell a healthy-but-idle relay
	// (no agents changing) from a wedged one. Stops when the event channel closes
	// (controller gone / `events_task` torn down).
	let heartbeat = {
		let tx = tx.clone();
		tokio::spawn(async move {
			let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
			ticker.tick().await; // the first tick fires immediately — skip it
			loop {
				ticker.tick().await;
				if tx.send(RelayEvent::Heartbeat).is_err() {
					break;
				}
			}
		})
	};

	// The controller's reflexive address, constant for the connection's lifetime —
	// captured once so each per-stream task (which moves its captures) doesn't need to
	// borrow `conn`, which the accept loop keeps using.
	let controller_addr = conn.remote_address();

	// Each stream the controller opens leads with a RelayRequest header: either route
	// it to one of this tenant's agents (the common case) or serve it ourselves (the
	// tenant's relay logs, a pairing slot, a punch).
	loop {
		let (mut c_send, mut c_recv) = match conn.accept_bi().await {
			Ok(pair) => pair,
			Err(_) => break,
		};
		let relay = relay.clone();
		let tenant = tenant.clone();
		tokio::spawn(async move {
			let Ok(req) = read_frame_capped::<_, RelayRequest>(&mut c_recv, MAX_CONTROL_FRAME).await else {
				return;
			};
			match req {
				// Served by the relay itself: hand back a snapshot of this tenant's own
				// log ring so its operator reads only its own activity — never another
				// tenant's — from the controller.
				RelayRequest::FetchLogs { after_seq } => {
					// No log line here: the controller polls this on a background timer,
					// so logging each fetch would spam the relay's own log (and feed back
					// into the controller via that very poll).
					let snapshot = tenant.log.snapshot_after(after_seq);
					let _ = write_frame(&mut c_send, &snapshot).await;
					let _ = c_send.finish();
				}
				// Pipe to the addressed agent within this tenant. Reset the stream with a
				// distinct code (rather than silently dropping it) when the agent is gone
				// or its connection is dying, so the controller gets a prompt, attributable
				// failure instead of an opaque close it might mistake for a transient
				// relay hiccup. A key that belongs to another tenant simply isn't found
				// here — routing is isolated.
				RelayRequest::Route { agent } => {
					let agent_key = crypto::canonical_pubkey(&agent).unwrap_or(agent);
					let Some(agent) = tenant.agent(&agent_key) else {
						tenant.log.debug(&format!(
							"route to offline agent {}… — resetting stream",
							key8(&agent_key)
						));
						let _ = c_send.reset(AGENT_UNAVAILABLE.into());
						return;
					};
					match agent.open_bi().await {
						Ok((a_send, a_recv)) => {
							tenant
								.log
								.debug(&format!("routing a stream to agent {}…", key8(&agent_key)));
							pipe(c_recv, a_send, a_recv, c_send).await;
							tenant
								.log
								.debug(&format!("routed stream to agent {}… closed", key8(&agent_key)));
						}
						Err(_) => {
							tenant.log.debug(&format!(
								"agent {}… connection unusable — resetting stream",
								key8(&agent_key)
							));
							let _ = c_send.reset(AGENT_UNAVAILABLE.into());
						}
					}
				}
				// Broker a peer-to-peer hole-punch to one of this tenant's agents: hand the
				// agent the controller's reflexive address over its signal channel, and
				// reply with the agent's. Both then try a direct QUIC path and upgrade off
				// the relay if it forms. The relay only exchanges addresses — the direct
				// connection is still authenticated end-to-end by the normal handshake.
				RelayRequest::Punch { agent } => {
					let agent_key = crypto::canonical_pubkey(&agent).unwrap_or(agent);
					let rendezvous = crypto::random_nonce_b64();
					let peer_addr = match tenant.agent_addr(&agent_key) {
						Some(agent_addr)
							if tenant.signal_agent(
								&agent_key,
								RelaySignal::Punch {
									controller_addr: controller_addr.to_string(),
									rendezvous: rendezvous.clone(),
								},
							) =>
						{
							tenant.log.debug(&format!(
								"brokering a punch: controller {controller_addr} ↔ agent {}… at {agent_addr}",
								key8(&agent_key)
							));
							Some(agent_addr.to_string())
						}
						// Agent offline, unknown, or its signal channel is gone: the
						// controller stays on the relay path.
						_ => None,
					};
					let _ = write_frame(&mut c_send, &PunchResponse { peer_addr, rendezvous }).await;
					let _ = c_send.finish();
				}
				// Park this stream as a pairing slot. The relay holds its halves until a
				// `Pairing` peer joins with the same nameplate (then pipes them) or the
				// slot expires; it never reads the PAKE/bundle that flow over the pipe.
				// Pairing slots are global (the parked stream is the controller's own), so
				// register on the relay rather than the tenant.
				RelayRequest::OpenPairing { nameplate } => {
					let parked = Pending {
						send: c_send,
						recv: c_recv,
						created: Instant::now(),
					};
					if relay.register_pairing(nameplate, parked) {
						tenant.log.debug("parked a pairing slot");
					} else {
						// On refusal `register_pairing` drops the `Pending`, closing the
						// controller's stream so its pairing attempt fails fast.
						tenant.log.warn("refused a pairing slot: at capacity");
					}
				}
			}
		});
	}

	tenant.log.info(&format!("controller session ended (gen {generation})"));
	// Only relinquish the slot if it's still ours: a reconnecting controller may
	// have already installed a newer sender, and clearing that would kill its
	// event stream and bounce it into an endless reconnect loop.
	{
		let mut slot = tenant.controller.lock().unwrap();
		if slot.as_ref().map(|s| s.generation) == Some(generation) {
			slot.take();
		}
	}
	events_task.abort();
	heartbeat.abort();
	Ok(())
}

/// Register an agent under its `tenant` and keep it reachable until its connection
/// drops. `_permit` ties an agent-pool slot to this connection's lifetime (see
/// CONTROLLER_RESERVED): it is released when the connection ends. `hello_send` is the
/// agent's hello-stream send half, reused to push relay→agent signals (hole-punch
/// requests) — the agent leaves that stream idle after registering.
async fn serve_agent(
	tenant: Arc<Tenant>,
	conn: quinn::Connection,
	public_key: String,
	mut hello_send: SendStream,
	_permit: OwnedSemaphorePermit,
) -> Result<()> {
	let conn_id = conn.stable_id();

	// Forward relay→agent signals onto the hello stream. Symmetric to the
	// controller's presence-event task; ends when the channel closes (agent gone).
	let (signals, mut rx) = tokio::sync::mpsc::unbounded_channel::<RelaySignal>();
	let signal_task = tokio::spawn(async move {
		while let Some(sig) = rx.recv().await {
			if write_frame(&mut hello_send, &sig).await.is_err() {
				break;
			}
		}
	});

	// Register in this tenant's agent map, displacing any prior connection under this
	// key. An honest agent holds exactly one connection; force-closing a stale
	// predecessor frees its slot/permit immediately instead of waiting for its own
	// `closed()` to fire, and bounds a single identity to one live connection.
	let previous = tenant.agents.lock().unwrap().insert(
		public_key.clone(),
		AgentHandle {
			conn: conn.clone(),
			signals,
		},
	);
	if let Some(prev) = previous {
		if prev.conn.stable_id() != conn_id {
			tenant.log.debug(&format!(
				"agent {}… reconnected — closing its previous connection",
				key8(&public_key)
			));
			prev.conn
				.close(0u32.into(), b"replaced by a newer connection for this key");
		}
	}
	tenant
		.log
		.info(&format!("agent {}… registered and reachable", key8(&public_key)));
	tenant.notify(RelayEvent::AgentOnline {
		public_key: public_key.clone(),
	});

	conn.closed().await;
	tenant
		.log
		.debug(&format!("agent {}… connection closed", key8(&public_key)));
	signal_task.abort();

	// Only deregister if we're still the registered connection for this key. A
	// reconnect can replace us with a fresh connection before our `closed()`
	// fires; removing then would wrongly mark a live agent offline (stale-cleanup
	// race), and the agent would stay unreachable until its new connection drops.
	let mut agents = tenant.agents.lock().unwrap();
	if agents.get(&public_key).map(|h| h.conn.stable_id()) == Some(conn_id) {
		agents.remove(&public_key);
		drop(agents);
		tenant
			.log
			.info(&format!("agent {}… deregistered (offline)", key8(&public_key)));
		tenant.notify(RelayEvent::AgentOffline { public_key });
	}
	Ok(())
}

/// Serve a `Pairing` join: take the controller's parked slot for this nameplate and
/// pipe the two streams together so the controller and the joining machine run the
/// PAKE end-to-end. If no live slot matches (wrong/expired nameplate, or it was
/// already claimed), reset the join stream with [`PAIRING_UNAVAILABLE`] so the
/// machine gets an attributable failure. The relay never reads what flows over the
/// pipe — only the nameplate matched here is ever visible to it.
async fn serve_pairing_join(relay: Relay, nameplate: String, mut send: SendStream, recv: RecvStream) -> Result<()> {
	let Some(slot) = relay.take_pairing(&nameplate) else {
		logbook::debug("pairing join with no matching slot — resetting");
		let _ = send.reset(PAIRING_UNAVAILABLE.into());
		return Ok(());
	};
	logbook::info("pairing slot matched — piping controller ↔ machine");
	// Pipe: controller (parked `slot`) ↔ joining machine (`send`/`recv`). The two ran
	// `RelayRequest::OpenPairing` / the pairing hello respectively, both already
	// stripped, so only their PAKE traffic flows.
	pipe(slot.recv, send, recv, slot.send).await;
	logbook::debug("pairing pipe closed");
	Ok(())
}

/// Pipe a controller stream and an agent stream together until both close.
/// Tearing both halves down as soon as one finishes would truncate a
/// request/response (the controller finishes its send half right after the
/// request) or end a live session early, so the shared helper waits for both —
/// see [`libretether_common::pipe_bidirectional`].
async fn pipe(c_recv: RecvStream, a_send: SendStream, a_recv: RecvStream, c_send: SendStream) {
	pipe_bidirectional(c_recv, c_send, a_recv, a_send).await;
}

#[cfg(test)]
mod tests {
	use super::*;
	use libretether_protocol::crypto::Identity;
	// The relay now binds via `tls::server_endpoint`; the test harness still spins up
	// bare loopback endpoints directly, so `Endpoint` is only referenced under test.
	use quinn::Endpoint;
	use std::net::Ipv4Addr;

	use std::sync::atomic::AtomicU64;

	/// A standalone agent-pool permit for tests that call `serve_agent` directly
	/// (in production the permit comes from the shared agent semaphore in `handle`).
	fn agent_permit() -> OwnedSemaphorePermit {
		Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap()
	}

	/// Unique suffix for each test relay's throwaway config path.
	static TEST_CFG_SEQ: AtomicU64 = AtomicU64::new(0);

	/// A throwaway config path unique to this test, so provision/revoke tests can
	/// persist without colliding.
	fn test_config_path() -> PathBuf {
		let n = TEST_CFG_SEQ.fetch_add(1, Ordering::Relaxed);
		std::env::temp_dir().join(format!("libretether-relay-test-{}-{n}.json", std::process::id()))
	}

	/// A relay for tests: a fixed admin secret, `open_registration` as given, no
	/// tenants, persisting to a unique throwaway temp path.
	fn test_relay(open_registration: bool) -> Relay {
		let cfg = ServerConfig {
			admin_secret: "admin-secret".into(),
			open_registration,
			tenants: Vec::new(),
			cert_der: String::new(),
			key_der: String::new(),
			portal: None,
		};
		Relay::new(&cfg, test_config_path())
	}

	/// Register a tenant with explicit secrets and return it (bypasses persistence),
	/// so routing tests can drive `serve_agent` / `serve_controller` directly.
	fn add_tenant(relay: &Relay, id: &str, owner: &str, agent: &str) -> Arc<Tenant> {
		let tenant = Tenant::from_record(&TenantRecord {
			id: id.into(),
			name: id.into(),
			owner_secret: owner.into(),
			agent_secret: agent.into(),
		});
		relay.tenants.lock().unwrap().insert(id.to_string(), tenant.clone());
		tenant
	}

	#[test]
	fn rate_limiter_allows_a_burst_then_sheds_per_source() {
		let relay = test_relay(false);
		let ip: IpAddr = "203.0.113.7".parse().unwrap();
		// Up to the window limit is allowed.
		for _ in 0..RATE_LIMIT_PER_WINDOW {
			assert!(relay.allow(ip));
		}
		// The next connection in the same window is shed.
		assert!(!relay.allow(ip));
		// A different source has its own independent budget.
		let other: IpAddr = "203.0.113.8".parse().unwrap();
		assert!(relay.allow(other));
	}

	/// A connected QUIC pair: the relay is the server (it accepts), the peer is the
	/// client (it opens the hello stream). Endpoints are returned so callers keep
	/// them (and the connections) alive for the test's duration.
	async fn loopback() -> (Endpoint, quinn::Connection, Endpoint, quinn::Connection) {
		tls::install_crypto_provider();
		let (cert, key) = tls::self_signed();
		let relay_ep = Endpoint::server(tls::server_config(cert, key), (Ipv4Addr::LOCALHOST, 0).into()).unwrap();
		let addr = relay_ep.local_addr().unwrap();
		let peer_ep = tls::client_endpoint(addr).unwrap();
		let accept = {
			let ep = relay_ep.clone();
			tokio::spawn(async move { ep.accept().await.unwrap().accept().unwrap().await.unwrap() })
		};
		let peer_conn = peer_ep.connect(addr, "libretether.local").unwrap().await.unwrap();
		let relay_conn = accept.await.unwrap();
		(relay_ep, relay_conn, peer_ep, peer_conn)
	}

	/// Drive the client side of the relay handshake for `role`/`secret` on `peer_conn`
	/// (signing the challenge with `id`), returning the relay's ack. Mirrors
	/// [`libretether_protocol::relay::client_handshake`] but lets a test present a
	/// mismatched proof.
	async fn client_hello(
		peer_conn: quinn::Connection,
		role: RelayRole,
		secret: &str,
		id: &Identity,
		proof_id: &Identity,
	) -> RelayAck {
		// `Identity` isn't `Clone`; reconstruct owned copies from their seeds so the
		// spawned client body can own them.
		let secret = secret.to_string();
		let id = Identity::from_seed_b64(&id.seed_b64()).unwrap();
		let proof_id = Identity::from_seed_b64(&proof_id.seed_b64()).unwrap();
		let (mut s, mut r) = peer_conn.open_bi().await.unwrap();
		write_frame(
			&mut s,
			&RelayHello {
				role,
				secret,
				public_key: id.public_b64(),
				nameplate: None,
			},
		)
		.await
		.unwrap();
		// A bad secret short-circuits to an ack with no challenge; only read a challenge
		// when we expect one. Peek by trying to read a challenge, then a proof.
		match read_frame_capped::<_, RelayChallenge>(&mut r, MAX_CONTROL_FRAME).await {
			Ok(ch) => {
				write_frame(
					&mut s,
					&RelayProof {
						signature: proof_id.sign_b64(ch.nonce.as_bytes()),
					},
				)
				.await
				.unwrap();
				read_frame_capped(&mut r, MAX_CONTROL_FRAME).await.unwrap()
			}
			// No challenge came back: the relay must have written the negative ack
			// directly. Re-read it from the same stream.
			Err(_) => RelayAck {
				accepted: false,
				reason: Some("bad secret".into()),
			},
		}
	}

	#[tokio::test]
	async fn authenticate_accepts_a_controller_with_owner_secret_and_valid_proof() {
		let (_rep, relay_conn, _pep, peer_conn) = loopback().await;
		let relay = test_relay(false);
		add_tenant(&relay, "t1", "owner-secret", "agent-secret");
		let id = Identity::generate();

		// Honest controller: the tenant's owner secret + a signature proving key ownership.
		let peer =
			tokio::spawn(async move { client_hello(peer_conn, RelayRole::Controller, "owner-secret", &id, &id).await });

		let authed = authenticate(&relay, &relay_conn)
			.await
			.unwrap()
			.expect("should authenticate");
		match authed {
			Authed::Controller { tenant, .. } => assert_eq!(tenant.id, "t1", "resolves to the owning tenant"),
			_ => panic!("expected a controller"),
		}
		assert!(peer.await.unwrap().accepted);
	}

	#[tokio::test]
	async fn authenticate_resolves_an_agent_to_its_tenant() {
		let (_rep, relay_conn, _pep, peer_conn) = loopback().await;
		let relay = test_relay(false);
		add_tenant(&relay, "t1", "owner-secret", "agent-secret");
		let id = Identity::generate();

		let peer =
			tokio::spawn(async move { client_hello(peer_conn, RelayRole::Agent, "agent-secret", &id, &id).await });

		let authed = authenticate(&relay, &relay_conn).await.unwrap().expect("authenticate");
		match authed {
			Authed::Agent { tenant, .. } => assert_eq!(tenant.id, "t1"),
			_ => panic!("expected an agent"),
		}
		assert!(peer.await.unwrap().accepted);
	}

	#[tokio::test]
	async fn authenticate_rejects_a_secret_no_tenant_claims() {
		let (_rep, relay_conn, _pep, peer_conn) = loopback().await;
		let relay = test_relay(false);
		add_tenant(&relay, "t1", "owner-secret", "agent-secret");
		let id = Identity::generate();

		let peer =
			tokio::spawn(async move { client_hello(peer_conn, RelayRole::Agent, "wrong-secret", &id, &id).await });

		assert!(authenticate(&relay, &relay_conn).await.unwrap().is_none());
		assert!(!peer.await.unwrap().accepted);
	}

	#[tokio::test]
	async fn authenticate_rejects_a_bad_key_ownership_proof() {
		let (_rep, relay_conn, _pep, peer_conn) = loopback().await;
		let relay = test_relay(false);
		add_tenant(&relay, "t1", "owner-secret", "agent-secret");
		let id = Identity::generate();
		let imposter = Identity::generate();

		// Correct agent secret but the proof is signed by a different key — so the
		// peer can't register under `id`'s routing key (the hijack the proof blocks).
		let peer =
			tokio::spawn(
				async move { client_hello(peer_conn, RelayRole::Agent, "agent-secret", &id, &imposter).await },
			);

		assert!(authenticate(&relay, &relay_conn).await.unwrap().is_none());
		assert!(!peer.await.unwrap().accepted);
	}

	#[tokio::test]
	async fn authenticate_admits_admin_with_the_admin_secret_as_full() {
		let (_rep, relay_conn, _pep, peer_conn) = loopback().await;
		let relay = test_relay(false);
		let id = Identity::generate();

		let peer =
			tokio::spawn(async move { client_hello(peer_conn, RelayRole::Admin, "admin-secret", &id, &id).await });

		match authenticate(&relay, &relay_conn).await.unwrap().expect("authenticate") {
			Authed::Admin { full, .. } => assert!(full, "the admin secret grants full rights"),
			_ => panic!("expected an admin"),
		}
		assert!(peer.await.unwrap().accepted);
	}

	#[tokio::test]
	async fn authenticate_rejects_admin_without_secret_unless_open_registration() {
		let id = Identity::generate();

		// Open registration off: a wrong admin secret is rejected outright.
		let (_rep, relay_conn, _pep, peer_conn) = loopback().await;
		let closed = test_relay(false);
		let seed = id.seed_b64();
		let peer = tokio::spawn(async move {
			let id = Identity::from_seed_b64(&seed).unwrap();
			client_hello(peer_conn, RelayRole::Admin, "nope", &id, &id).await
		});
		assert!(authenticate(&closed, &relay_conn).await.unwrap().is_none());
		assert!(!peer.await.unwrap().accepted);

		// Open registration on: the same client is admitted, but only registration-only.
		let (_rep, relay_conn, _pep, peer_conn) = loopback().await;
		let open = test_relay(true);
		let peer = tokio::spawn(async move { client_hello(peer_conn, RelayRole::Admin, "nope", &id, &id).await });
		match authenticate(&open, &relay_conn).await.unwrap().expect("authenticate") {
			Authed::Admin { full, .. } => assert!(!full, "open registration admits provision-only"),
			_ => panic!("expected an admin"),
		}
		assert!(peer.await.unwrap().accepted);
	}

	// ------------------------------------------------------------ rate limiter

	#[test]
	fn rate_limiter_resets_after_the_window_elapses() {
		let relay = test_relay(false);
		let ip: IpAddr = "203.0.113.9".parse().unwrap();
		let t0 = Instant::now();
		// Exhaust the window's budget at a fixed instant.
		for _ in 0..RATE_LIMIT_PER_WINDOW {
			assert!(relay.allow_at(ip, t0));
		}
		assert!(!relay.allow_at(ip, t0), "shed once the window budget is spent");
		// A check past the window rolls over to a fresh budget.
		let t1 = t0 + RATE_WINDOW + Duration::from_millis(1);
		assert!(relay.allow_at(ip, t1), "a new window grants a fresh budget");
	}

	#[test]
	fn rate_limiter_evicts_stale_entries_when_the_map_grows() {
		let relay = test_relay(false);
		let t0 = Instant::now();
		// Seed the limiter past its eviction threshold with stale entries.
		{
			let mut map = relay.limiter.lock().unwrap();
			for i in 0..10_001u32 {
				map.insert(IpAddr::from(std::net::Ipv4Addr::from(i)), (1, t0));
			}
		}
		// A check well past the window triggers the opportunistic retain.
		let later = t0 + RATE_WINDOW + Duration::from_secs(1);
		assert!(relay.allow_at("198.51.100.1".parse().unwrap(), later));
		let len = relay.limiter.lock().unwrap().len();
		assert!(len < 10_001, "stale entries should be evicted, map still has {len}");
	}

	// ------------------------------------------------------ routing harness

	/// A relay server endpoint bound to loopback, plus its address.
	fn relay_server() -> (Endpoint, SocketAddr) {
		tls::install_crypto_provider();
		let (cert, key) = tls::self_signed();
		let ep = Endpoint::server(tls::server_config(cert, key), (Ipv4Addr::LOCALHOST, 0).into()).unwrap();
		let addr = ep.local_addr().unwrap();
		(ep, addr)
	}

	/// Dial `relay_ep` from a fresh client; returns the client endpoint (the caller
	/// keeps it alive), the client's connection, and the relay's view of it.
	async fn connect(relay_ep: &Endpoint, addr: SocketAddr) -> (Endpoint, quinn::Connection, quinn::Connection) {
		let client_ep = tls::client_endpoint(addr).unwrap();
		let accept = {
			let ep = relay_ep.clone();
			tokio::spawn(async move { ep.accept().await.unwrap().accept().unwrap().await.unwrap() })
		};
		let client_conn = client_ep.connect(addr, "libretether.local").unwrap().await.unwrap();
		let relay_view = accept.await.unwrap();
		(client_ep, client_conn, relay_view)
	}

	/// Open the controller's "hello" stream and hand back the relay-side send half
	/// `serve_controller` writes presence events on, plus the client-side recv half
	/// the controller reads them from. (Auth is exercised separately; this skips it
	/// to test routing in isolation.)
	async fn open_events(
		ctrl_conn: &quinn::Connection,
		ctrl_view: &quinn::Connection,
	) -> (quinn::SendStream, quinn::RecvStream) {
		let (mut hello_send, hello_recv) = ctrl_conn.open_bi().await.unwrap();
		hello_send.write_all(b"\x00").await.unwrap(); // materialize the stream so the relay accepts it
		let (events_send, _events_recv) = ctrl_view.accept_bi().await.unwrap();
		(events_send, hello_recv)
	}

	/// Open the agent's "hello" stream and hand back the relay-side send half
	/// `serve_agent` pushes signals on, plus the agent-side recv half it reads them
	/// from — the peer-to-peer signal channel. Mirrors [`open_events`] for the agent.
	async fn open_agent_hello(
		agent_conn: &quinn::Connection,
		agent_view: &quinn::Connection,
	) -> (quinn::SendStream, quinn::RecvStream) {
		let (mut a_send, a_recv) = agent_conn.open_bi().await.unwrap();
		a_send.write_all(b"\x00").await.unwrap(); // materialize so the relay accepts it
		let (relay_send, _relay_recv) = agent_view.accept_bi().await.unwrap();
		(relay_send, a_recv)
	}

	/// Poll `cond` until true, failing the test if it never becomes true.
	async fn wait_until(mut cond: impl FnMut() -> bool) {
		for _ in 0..400 {
			if cond() {
				return;
			}
			tokio::time::sleep(Duration::from_millis(5)).await;
		}
		panic!("condition was not met within the timeout");
	}

	async fn with_timeout<T>(label: &str, fut: impl std::future::Future<Output = T>) -> T {
		tokio::time::timeout(Duration::from_secs(5), fut)
			.await
			.unwrap_or_else(|_| panic!("{label} timed out"))
	}

	#[tokio::test]
	async fn routes_a_controller_stream_to_the_addressed_agent_both_ways() {
		let (relay_ep, addr) = relay_server();
		let relay = test_relay(false);
		let tenant = add_tenant(&relay, "t1", "owner-secret", "agent-secret");
		let agent_key = "AGENT_PUBKEY".to_string();

		// Register an agent under the tenant.
		let (_aep, agent_conn, agent_view) = connect(&relay_ep, addr).await;
		let (hello_send, _sig_recv) = open_agent_hello(&agent_conn, &agent_view).await;
		tokio::spawn({
			let (tenant, key) = (tenant.clone(), agent_key.clone());
			async move { serve_agent(tenant, agent_view, key, hello_send, agent_permit()).await }
		});
		wait_until(|| tenant.agent(&agent_key).is_some()).await;

		// Bring up the tenant's controller and start serving it.
		let (_cep, ctrl_conn, ctrl_view) = connect(&relay_ep, addr).await;
		let (events_send, _events_recv) = open_events(&ctrl_conn, &ctrl_view).await;
		tokio::spawn({
			let (relay, tenant) = (relay.clone(), tenant.clone());
			async move { serve_controller(relay, tenant, ctrl_view, events_send).await }
		});

		// Controller opens a routed stream to the agent and sends a payload.
		let (mut rsend, mut rrecv) = ctrl_conn.open_bi().await.unwrap();
		write_frame(
			&mut rsend,
			&RelayRequest::Route {
				agent: agent_key.clone(),
			},
		)
		.await
		.unwrap();
		rsend.write_all(b"PING").await.unwrap();
		let _ = rsend.finish();

		// The agent receives the payload *without* the RelayRequest header (the relay
		// consumed it), then echoes back through the relay to the controller.
		let (mut asend, mut arecv) = with_timeout("agent accept", agent_conn.accept_bi()).await.unwrap();
		let got = with_timeout("agent read", arecv.read_to_end(64)).await.unwrap();
		assert_eq!(
			got, b"PING",
			"the RelayRequest header must be stripped; agent sees only the payload"
		);
		asend.write_all(b"PONG").await.unwrap();
		let _ = asend.finish();

		let back = with_timeout("controller read", rrecv.read_to_end(64)).await.unwrap();
		assert_eq!(back, b"PONG");
	}

	#[tokio::test]
	async fn a_routed_stream_for_an_unknown_agent_is_dropped() {
		let (relay_ep, addr) = relay_server();
		let relay = test_relay(false);
		let tenant = add_tenant(&relay, "t1", "owner-secret", "agent-secret");

		let (_cep, ctrl_conn, ctrl_view) = connect(&relay_ep, addr).await;
		let (events_send, _events_recv) = open_events(&ctrl_conn, &ctrl_view).await;
		tokio::spawn({
			let (relay, tenant) = (relay.clone(), tenant.clone());
			async move { serve_controller(relay, tenant, ctrl_view, events_send).await }
		});

		// Route to an agent that was never registered: the relay resets the stream
		// with AGENT_UNAVAILABLE, so the controller gets an attributable failure
		// rather than an ambiguous clean close.
		let (mut rsend, mut rrecv) = ctrl_conn.open_bi().await.unwrap();
		write_frame(&mut rsend, &RelayRequest::Route { agent: "GHOST".into() })
			.await
			.unwrap();
		rsend.write_all(b"hello?").await.unwrap();
		let _ = rsend.finish();
		let result = with_timeout("read after route-to-unknown", rrecv.read_to_end(64)).await;
		match result {
			Err(quinn::ReadToEndError::Read(quinn::ReadError::Reset(code))) => {
				assert_eq!(
					code,
					AGENT_UNAVAILABLE.into(),
					"reset carries the agent-unavailable code"
				);
			}
			other => panic!("expected a stream reset for an unknown agent, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn a_fetch_logs_request_is_served_by_the_relay_itself() {
		let (relay_ep, addr) = relay_server();
		let relay = test_relay(false);
		let tenant = add_tenant(&relay, "t1", "owner-secret", "agent-secret");

		// Seed *this tenant's* log ring so the snapshot has something to return — a
		// controller only ever fetches its own tenant's lines, never the global log.
		tenant.log.info("tenant activity line");

		let (_cep, ctrl_conn, ctrl_view) = connect(&relay_ep, addr).await;
		let (events_send, _events_recv) = open_events(&ctrl_conn, &ctrl_view).await;
		tokio::spawn({
			let (relay, tenant) = (relay.clone(), tenant.clone());
			async move { serve_controller(relay, tenant, ctrl_view, events_send).await }
		});

		// A FetchLogs stream is answered by the relay itself (not routed to an agent):
		// it replies with a LogsResult drawn from this tenant's log buffer.
		let (mut rsend, mut rrecv) = ctrl_conn.open_bi().await.unwrap();
		write_frame(&mut rsend, &RelayRequest::FetchLogs { after_seq: None })
			.await
			.unwrap();
		let _ = rsend.finish();
		let result: libretether_protocol::LogsResult =
			with_timeout("read relay logs", read_frame_capped(&mut rrecv, MAX_CONTROL_FRAME))
				.await
				.unwrap();
		assert!(
			result.lines.iter().any(|l| l.message.contains("tenant activity line")),
			"the relay returns this tenant's own recorded log lines"
		);
	}

	#[test]
	fn info_load_refuses_to_create_a_missing_config() {
		// `info` reads through `load`, which must fail with an actionable error
		// (rather than silently minting secrets) when no config exists, and must
		// leave no file behind — printing secrets the running relay never used
		// would be a footgun.
		let path = std::env::temp_dir()
			.join(format!("libretether-relay-load-test-{}", std::process::id()))
			.join("config.json");
		let _ = std::fs::remove_dir_all(path.parent().unwrap());
		// Not `unwrap_err`: `ServerConfig` deliberately isn't `Debug` (it holds the
		// secrets), so match rather than require a `Debug` bound on the Ok type.
		let err = match load(&path) {
			Ok(_) => panic!("load must fail when no config exists"),
			Err(e) => e.to_string(),
		};
		assert!(
			err.contains("no relay config"),
			"expected an actionable error, got: {err}"
		);
		assert!(!path.exists(), "load must not create a config file");
	}

	#[test]
	fn config_validate_rejects_a_blank_secret() {
		let mut cfg = ServerConfig::generate();
		assert!(cfg.validate().is_ok(), "a freshly generated config is valid");
		cfg.admin_secret = "   ".into();
		assert!(cfg.validate().is_err(), "a blank admin secret must be rejected");

		// A tenant with a blank owner/agent secret is also rejected (a fail-open, since
		// `ct_eq("", "")` would authenticate an empty-secret peer).
		let mut cfg = ServerConfig::generate();
		cfg.tenants.push(TenantRecord {
			id: "t1".into(),
			name: "team".into(),
			owner_secret: String::new(),
			agent_secret: "agent".into(),
		});
		assert!(
			cfg.validate().is_err(),
			"a tenant with a blank owner secret must be rejected"
		);
	}

	#[test]
	fn config_generate_has_no_tenants_and_open_registration_off() {
		let cfg = ServerConfig::generate();
		assert!(cfg.tenants.is_empty(), "a fresh relay starts with no tenants");
		assert!(!cfg.open_registration, "open registration is off by default");
		assert!(!cfg.admin_secret.trim().is_empty(), "an admin secret is minted");
	}

	#[test]
	fn resolve_listen_prefers_flag_then_env_then_dual_stack_default() {
		// Default is dual-stack, so the image binds IPv6 (+ IPv4-mapped) with no flag.
		assert_eq!(resolve_listen_inner(None, None), format!("[::]:{DEFAULT_PORT}"));
		// The env var (e.g. from docker-compose) is honored when there's no flag.
		assert_eq!(resolve_listen_inner(None, Some("0.0.0.0:1234".into())), "0.0.0.0:1234");
		// The flag wins over the env.
		assert_eq!(
			resolve_listen_inner(Some("[::1]:9000".into()), Some("0.0.0.0:1234".into())),
			"[::1]:9000"
		);
		// Blank values fall through to the next source.
		assert_eq!(
			resolve_listen_inner(Some("  ".into()), None),
			format!("[::]:{DEFAULT_PORT}")
		);
	}

	#[tokio::test]
	async fn a_new_controller_displaces_and_closes_the_previous_one() {
		let (relay_ep, addr) = relay_server();
		let relay = test_relay(false);
		let tenant = add_tenant(&relay, "t1", "owner-secret", "agent-secret");

		// Controller A.
		let (_aep, ctrl_a, view_a) = connect(&relay_ep, addr).await;
		let (events_a, _ra) = open_events(&ctrl_a, &view_a).await;
		tokio::spawn({
			let (relay, tenant) = (relay.clone(), tenant.clone());
			async move { serve_controller(relay, tenant, view_a, events_a).await }
		});
		wait_until(|| tenant.controller.lock().unwrap().is_some()).await;
		let gen_a = tenant.controller.lock().unwrap().as_ref().unwrap().generation;

		// Controller B connects and must displace A.
		let (_bep, ctrl_b, view_b) = connect(&relay_ep, addr).await;
		let (events_b, _rb) = open_events(&ctrl_b, &view_b).await;
		tokio::spawn({
			let (relay, tenant) = (relay.clone(), tenant.clone());
			async move { serve_controller(relay, tenant, view_b, events_b).await }
		});
		wait_until(|| tenant.controller.lock().unwrap().as_ref().map(|s| s.generation) != Some(gen_a)).await;

		// A's connection is force-closed by the relay (no zombie routing loop).
		with_timeout("controller A closed", ctrl_a.closed()).await;
	}

	#[tokio::test]
	async fn a_reconnecting_agent_keeps_the_newer_connection_registered() {
		let (relay_ep, addr) = relay_server();
		let relay = test_relay(false);
		let tenant = add_tenant(&relay, "t1", "owner-secret", "agent-secret");
		let key = "AGENT".to_string();

		// First connection C1 registers under the key.
		let (_e1, agent1, view1) = connect(&relay_ep, addr).await;
		let id1 = view1.stable_id();
		let (hs1, _s1) = open_agent_hello(&agent1, &view1).await;
		tokio::spawn({
			let (tenant, key) = (tenant.clone(), key.clone());
			async move { serve_agent(tenant, view1, key, hs1, agent_permit()).await }
		});
		wait_until(|| tenant.agent(&key).map(|c| c.stable_id()) == Some(id1)).await;

		// C2 (a reconnect) registers under the same key, replacing C1 in the map.
		let (_e2, agent2, view2) = connect(&relay_ep, addr).await;
		let id2 = view2.stable_id();
		assert_ne!(id1, id2);
		let (hs2, _s2) = open_agent_hello(&agent2, &view2).await;
		tokio::spawn({
			let (tenant, key) = (tenant.clone(), key.clone());
			async move { serve_agent(tenant, view2, key, hs2, agent_permit()).await }
		});
		wait_until(|| tenant.agent(&key).map(|c| c.stable_id()) == Some(id2)).await;

		// Now C1 drops. Its teardown must NOT deregister the key — the live
		// connection is C2 (the stable-id guard). C2 stays reachable.
		agent1.close(0u32.into(), b"bye");
		tokio::time::sleep(Duration::from_millis(100)).await;
		assert_eq!(
			tenant.agent(&key).map(|c| c.stable_id()),
			Some(id2),
			"the reconnected agent (C2) must remain registered after the stale C1 drops"
		);
		let _ = agent2; // kept alive for the duration of the test
	}

	#[tokio::test]
	async fn controller_is_notified_when_an_agent_comes_online_and_goes_offline() {
		let (relay_ep, addr) = relay_server();
		let relay = test_relay(false);
		let tenant = add_tenant(&relay, "t1", "owner-secret", "agent-secret");

		// Controller attaches and starts reading presence events.
		let (_cep, ctrl_conn, ctrl_view) = connect(&relay_ep, addr).await;
		let (events_send, mut hello_recv) = open_events(&ctrl_conn, &ctrl_view).await;
		tokio::spawn({
			let (relay, tenant) = (relay.clone(), tenant.clone());
			async move { serve_controller(relay, tenant, ctrl_view, events_send).await }
		});
		wait_until(|| tenant.controller.lock().unwrap().is_some()).await;

		// Agent comes online → AgentOnline reaches the controller.
		let key = "AGENT".to_string();
		let (_aep, agent_conn, agent_view) = connect(&relay_ep, addr).await;
		let (hello_send, _sig_recv) = open_agent_hello(&agent_conn, &agent_view).await;
		tokio::spawn({
			let (tenant, key) = (tenant.clone(), key.clone());
			async move { serve_agent(tenant, agent_view, key, hello_send, agent_permit()).await }
		});
		let online: RelayEvent = with_timeout("online event", read_frame_capped(&mut hello_recv, MAX_CONTROL_FRAME))
			.await
			.unwrap();
		assert!(matches!(online, RelayEvent::AgentOnline { public_key } if public_key == key));

		// Agent drops → AgentOffline reaches the controller.
		agent_conn.close(0u32.into(), b"bye");
		let offline: RelayEvent = with_timeout("offline event", read_frame_capped(&mut hello_recv, MAX_CONTROL_FRAME))
			.await
			.unwrap();
		assert!(matches!(offline, RelayEvent::AgentOffline { public_key } if public_key == key));
	}

	// ------------------------------------------------------ p2p punch brokering

	#[tokio::test]
	async fn relay_brokers_a_punch_swapping_the_two_reflexive_addresses() {
		let (relay_ep, addr) = relay_server();
		let relay = test_relay(false);
		let tenant = add_tenant(&relay, "t1", "owner-secret", "agent-secret");
		let agent_key = "AGENT_PUBKEY".to_string();

		// Register an agent, keeping its signal channel so we can observe the punch.
		let (_aep, agent_conn, agent_view) = connect(&relay_ep, addr).await;
		let agent_reflexive = agent_view.remote_address();
		let (hello_send, mut sig_recv) = open_agent_hello(&agent_conn, &agent_view).await;
		tokio::spawn({
			let (tenant, key) = (tenant.clone(), agent_key.clone());
			async move { serve_agent(tenant, agent_view, key, hello_send, agent_permit()).await }
		});
		wait_until(|| tenant.agent(&agent_key).is_some()).await;

		// Controller.
		let (_cep, ctrl_conn, ctrl_view) = connect(&relay_ep, addr).await;
		let ctrl_reflexive = ctrl_view.remote_address();
		let (events_send, _er) = open_events(&ctrl_conn, &ctrl_view).await;
		tokio::spawn({
			let (relay, tenant) = (relay.clone(), tenant.clone());
			async move { serve_controller(relay, tenant, ctrl_view, events_send).await }
		});
		wait_until(|| tenant.controller.lock().unwrap().is_some()).await;

		// Controller asks the relay to broker a punch; it learns the agent's address.
		let resp = with_timeout(
			"punch response",
			libretether_protocol::relay::request_punch(&ctrl_conn, &agent_key),
		)
		.await
		.unwrap();
		assert_eq!(
			resp.peer_addr.as_deref(),
			Some(agent_reflexive.to_string().as_str()),
			"the controller is told the agent's reflexive address"
		);

		// The agent receives a matching signal telling it the controller's address, with
		// the same rendezvous id — so the two coordinate the same punch.
		let sig: RelaySignal = with_timeout("agent signal", read_frame_capped(&mut sig_recv, MAX_CONTROL_FRAME))
			.await
			.unwrap();
		let RelaySignal::Punch {
			controller_addr,
			rendezvous,
		} = sig;
		assert_eq!(
			controller_addr,
			ctrl_reflexive.to_string(),
			"the agent is told the controller's reflexive address"
		);
		assert_eq!(rendezvous, resp.rendezvous, "both sides share one rendezvous id");
	}

	#[tokio::test]
	async fn a_punch_for_an_offline_agent_brokers_no_address() {
		let (relay_ep, addr) = relay_server();
		let relay = test_relay(false);
		let tenant = add_tenant(&relay, "t1", "owner-secret", "agent-secret");

		let (_cep, ctrl_conn, ctrl_view) = connect(&relay_ep, addr).await;
		let (events_send, _er) = open_events(&ctrl_conn, &ctrl_view).await;
		tokio::spawn({
			let (relay, tenant) = (relay.clone(), tenant.clone());
			async move { serve_controller(relay, tenant, ctrl_view, events_send).await }
		});
		wait_until(|| tenant.controller.lock().unwrap().is_some()).await;

		// No agent is registered: the relay can't broker a punch, so the controller is
		// told to stay on the relay path (peer_addr None) rather than hanging.
		let resp = with_timeout(
			"punch response",
			libretether_protocol::relay::request_punch(&ctrl_conn, "GHOST"),
		)
		.await
		.unwrap();
		assert!(resp.peer_addr.is_none(), "no address is brokered for an offline agent");
	}

	// ------------------------------------------------------ pairing mailbox

	use libretether_protocol::pairing::{self, PairBundle, PairingCode};

	fn sample_bundle() -> PairBundle {
		PairBundle {
			enrollment_token: "tok-xyz".into(),
			controller_key: "Q29udHJvbGxlcktleQ==".into(),
			agent_secret: "agent-sekret".into(),
			name: Some("kitchen-pc".into()),
		}
	}

	/// A live relay-side stream pair, for constructing a [`Pending`] in unit tests.
	async fn stream_pair(client: &quinn::Connection, server: &quinn::Connection) -> (SendStream, RecvStream) {
		let (mut cs, _cr) = client.open_bi().await.unwrap();
		cs.write_all(b"\x00").await.unwrap(); // materialize so the server can accept it
		server.accept_bi().await.unwrap()
	}

	#[tokio::test]
	async fn take_pairing_is_single_use_and_honors_the_ttl() {
		let (_rep, relay_conn, _pep, peer_conn) = loopback().await;
		let relay = test_relay(false);
		let (s, r) = stream_pair(&peer_conn, &relay_conn).await;
		let t0 = Instant::now();
		assert!(relay.register_pairing_at(
			"NP1".into(),
			Pending {
				send: s,
				recv: r,
				created: t0
			},
			t0
		));

		// A second take finds nothing (single-use), and an unrelated nameplate misses.
		assert!(relay.take_pairing_at("ghost", t0).is_none());
		assert!(
			relay.take_pairing_at("NP1", t0).is_some(),
			"the live slot is taken once"
		);
		assert!(
			relay.take_pairing_at("NP1", t0).is_none(),
			"and is gone after being claimed"
		);

		// A slot older than the TTL is treated as absent (and dropped).
		let (s2, r2) = stream_pair(&peer_conn, &relay_conn).await;
		assert!(relay.register_pairing_at(
			"NP2".into(),
			Pending {
				send: s2,
				recv: r2,
				created: t0
			},
			t0
		));
		let expired = t0 + PAIRING_TTL + Duration::from_secs(1);
		assert!(
			relay.take_pairing_at("NP2", expired).is_none(),
			"an expired slot is not returned"
		);
	}

	#[tokio::test]
	async fn register_pairing_caps_live_slots_but_allows_overwriting_one() {
		let (_rep, relay_conn, _pep, peer_conn) = loopback().await;
		let relay = test_relay(false);
		let t0 = Instant::now();
		for i in 0..MAX_PENDING_PAIRINGS {
			let (s, r) = stream_pair(&peer_conn, &relay_conn).await;
			assert!(relay.register_pairing_at(
				format!("slot{i}"),
				Pending {
					send: s,
					recv: r,
					created: t0
				},
				t0
			));
		}
		// At capacity: a brand-new nameplate is refused…
		let (s, r) = stream_pair(&peer_conn, &relay_conn).await;
		assert!(!relay.register_pairing_at(
			"overflow".into(),
			Pending {
				send: s,
				recv: r,
				created: t0
			},
			t0
		));
		// …but reusing an existing nameplate (last-writer-wins) is allowed.
		let (s, r) = stream_pair(&peer_conn, &relay_conn).await;
		assert!(relay.register_pairing_at(
			"slot0".into(),
			Pending {
				send: s,
				recv: r,
				created: t0
			},
			t0
		));
		// And once a window passes, the sweep frees them so new slots fit again.
		relay.sweep_pairings_at(t0 + PAIRING_TTL + Duration::from_secs(1));
		assert!(
			relay.pairings.lock().unwrap().is_empty(),
			"the sweep drops every expired slot"
		);
	}

	#[tokio::test]
	async fn a_pairing_join_with_no_slot_is_reset_with_its_own_code() {
		let (relay_ep, addr) = relay_server();
		let relay = test_relay(false);

		// A machine opens a pairing stream the relay has no slot for.
		let (_jep, join_conn, join_view) = connect(&relay_ep, addr).await;
		let join = tokio::spawn(async move {
			let (mut s, mut r) = join_conn.open_bi().await.unwrap();
			write_frame(
				&mut s,
				&RelayHello {
					role: RelayRole::Pairing,
					secret: String::new(),
					public_key: String::new(),
					nameplate: Some("GHOST".into()),
				},
			)
			.await
			.unwrap();
			// The relay strips the hello then resets — the next read carries the code.
			let res = r.read_to_end(64).await;
			(res, join_conn)
		});

		// Relay side: read the pairing hello and route it through serve_pairing_join.
		let (rs, mut rr) = join_view.accept_bi().await.unwrap();
		let hello: RelayHello = read_frame_capped(&mut rr, MAX_CONTROL_FRAME).await.unwrap();
		assert!(matches!(hello.role, RelayRole::Pairing));
		serve_pairing_join(relay, hello.nameplate.unwrap(), rs, rr)
			.await
			.unwrap();

		let (res, _conn) = with_timeout("join reset", join).await.unwrap();
		match res {
			Err(quinn::ReadToEndError::Read(quinn::ReadError::Reset(code))) => {
				assert_eq!(
					code,
					PAIRING_UNAVAILABLE.into(),
					"an unmatched join is reset with the pairing code"
				);
			}
			other => panic!("expected a reset for a missing pairing slot, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn pairing_mailbox_pipes_a_full_pake_end_to_end() {
		// The whole point, end to end: a controller parks a slot, a not-yet-enrolled
		// machine joins by nameplate, and the real PAKE runs over the relay-piped
		// channel — delivering the bundle without the relay seeing the password.
		let (relay_ep, addr) = relay_server();
		let relay = test_relay(false);
		let tenant = add_tenant(&relay, "t1", "owner-secret", "agent-secret");
		let code = PairingCode::generate();
		let bundle = sample_bundle();

		// Controller connection, served so it can open a pairing slot.
		let (_cep, ctrl_conn, ctrl_view) = connect(&relay_ep, addr).await;
		let (events_send, _er) = open_events(&ctrl_conn, &ctrl_view).await;
		tokio::spawn({
			let (relay, tenant) = (relay.clone(), tenant.clone());
			async move { serve_controller(relay, tenant, ctrl_view, events_send).await }
		});

		// Controller opens the pairing slot and runs its side of the PAKE.
		let ctrl_task = {
			let (code, bundle) = (code.clone(), bundle.clone());
			tokio::spawn(async move {
				let (mut cs, mut cr) = ctrl_conn.open_bi().await.unwrap();
				write_frame(
					&mut cs,
					&RelayRequest::OpenPairing {
						nameplate: code.nameplate.clone(),
					},
				)
				.await
				.unwrap();
				let phrase = pairing::controller_pair(&mut cs, &mut cr, &code, &bundle).await;
				(phrase, ctrl_conn)
			})
		};
		wait_until({
			let (relay, np) = (relay.clone(), code.nameplate.clone());
			move || relay.pairings.lock().unwrap().contains_key(&np)
		})
		.await;

		// The machine joins by nameplate; drive the relay's pairing side, then run the
		// agent's side of the PAKE on the client connection.
		let (_jep, join_conn, join_view) = connect(&relay_ep, addr).await;
		tokio::spawn({
			let relay = relay.clone();
			async move {
				let (rs, mut rr) = join_view.accept_bi().await.unwrap();
				let hello: RelayHello = read_frame_capped(&mut rr, MAX_CONTROL_FRAME).await.unwrap();
				serve_pairing_join(relay, hello.nameplate.unwrap(), rs, rr)
					.await
					.unwrap();
			}
		});
		let agent_task = {
			let code = code.clone();
			tokio::spawn(async move {
				let (mut s, mut r) = libretether_protocol::relay::pairing_join(&join_conn, &code.nameplate)
					.await
					.unwrap();
				let out = pairing::agent_pair(&mut s, &mut r, &code).await;
				(out, join_conn)
			})
		};

		let (ctrl_phrase, _c) = with_timeout("controller pair", ctrl_task).await.unwrap();
		let (agent_out, _j) = with_timeout("agent pair", agent_task).await.unwrap();
		let ctrl_phrase = ctrl_phrase.expect("controller side succeeds");
		let (got_bundle, agent_phrase) = agent_out.expect("agent side succeeds");
		assert_eq!(
			got_bundle, bundle,
			"the agent receives the exact bundle through the relay"
		);
		assert_eq!(ctrl_phrase, agent_phrase, "both ends show the same verify phrase");
	}

	// ------------------------------------------------------ multi-tenant provisioning

	#[test]
	fn provision_mints_distinct_tenants_persists_and_resolves_by_secret() {
		// Build a relay with a real config path so we can confirm the mint is persisted.
		let path = test_config_path();
		let _ = std::fs::remove_file(&path);
		let cfg = ServerConfig::generate();
		let admin = cfg.admin_secret.clone();
		let relay = Relay::new(&cfg, path.clone());

		let a = relay.provision("team-a".into()).expect("provision a");
		let b = relay.provision("team-b".into()).expect("provision b");
		assert_ne!(a.tenant_id, b.tenant_id, "each tenant gets a distinct id");
		assert_ne!(a.owner_secret, b.owner_secret, "and distinct owner secrets");
		assert_ne!(a.agent_secret, b.agent_secret, "and distinct agent secrets");

		// The minted secrets resolve back to their own tenant, and don't cross over.
		assert_eq!(relay.tenant_by_owner(&a.owner_secret).unwrap().id, a.tenant_id);
		assert_eq!(relay.tenant_by_agent(&b.agent_secret).unwrap().id, b.tenant_id);
		assert!(
			relay.tenant_by_owner(&a.agent_secret).is_none(),
			"an agent secret is not an owner secret"
		);

		// Persisted: reloading the config yields both tenants and the same admin secret.
		let reloaded = parse_config(&std::fs::read_to_string(&path).unwrap(), &path).unwrap();
		assert_eq!(reloaded.admin_secret, admin);
		assert_eq!(reloaded.tenants.len(), 2, "both tenants are persisted");
		let _ = std::fs::remove_file(&path);
	}

	#[test]
	fn provision_rejects_a_blank_name() {
		let relay = test_relay(false);
		assert!(relay.provision("   ".into()).is_err(), "a blank tenant name is refused");
		assert!(relay.tenants.lock().unwrap().is_empty(), "and no tenant is created");
	}

	#[test]
	fn revoke_removes_a_tenant_and_is_idempotent() {
		let relay = test_relay(false);
		let creds = relay.provision("team".into()).unwrap();
		assert!(relay.tenant_by_owner(&creds.owner_secret).is_some());

		assert!(relay.revoke(&creds.tenant_id).unwrap(), "the live tenant is removed");
		assert!(
			relay.tenant_by_owner(&creds.owner_secret).is_none(),
			"its owner secret no longer resolves"
		);
		assert!(
			!relay.revoke(&creds.tenant_id).unwrap(),
			"revoking again reports it was absent"
		);
	}

	#[test]
	fn handle_admin_gates_management_ops_for_registration_only_clients() {
		let relay = test_relay(true);

		// A registration-only client (open registration) may provision…
		let resp = handle_admin(&relay, false, AdminRequest::Provision { name: "self".into() });
		assert!(
			matches!(resp, AdminResponse::Provisioned(_)),
			"open registration can provision"
		);

		// …but may not list or revoke.
		assert!(matches!(
			handle_admin(&relay, false, AdminRequest::List),
			AdminResponse::Error { .. }
		));
		assert!(matches!(
			handle_admin(&relay, false, AdminRequest::Revoke { tenant_id: "x".into() }),
			AdminResponse::Error { .. }
		));

		// A full admin can list (and sees the provisioned tenant).
		match handle_admin(&relay, true, AdminRequest::List) {
			AdminResponse::Tenants { tenants } => assert_eq!(tenants.len(), 1),
			other => panic!("expected a tenant list, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn routing_is_isolated_between_tenants() {
		// An agent registered under tenant A must be unreachable from tenant B's
		// controller — the core multi-tenant isolation guarantee.
		let (relay_ep, addr) = relay_server();
		let relay = test_relay(false);
		let tenant_a = add_tenant(&relay, "a", "owner-a", "agent-a");
		let tenant_b = add_tenant(&relay, "b", "owner-b", "agent-b");
		let agent_key = "AGENT_IN_A".to_string();

		// Register the agent under tenant A.
		let (_aep, agent_conn, agent_view) = connect(&relay_ep, addr).await;
		let (hello_send, _sig) = open_agent_hello(&agent_conn, &agent_view).await;
		tokio::spawn({
			let (tenant, key) = (tenant_a.clone(), agent_key.clone());
			async move { serve_agent(tenant, agent_view, key, hello_send, agent_permit()).await }
		});
		wait_until(|| tenant_a.agent(&agent_key).is_some()).await;

		// Tenant B's controller tries to route to A's agent by key.
		let (_cep, ctrl_conn, ctrl_view) = connect(&relay_ep, addr).await;
		let (events_send, _er) = open_events(&ctrl_conn, &ctrl_view).await;
		tokio::spawn({
			let (relay, tenant) = (relay.clone(), tenant_b.clone());
			async move { serve_controller(relay, tenant, ctrl_view, events_send).await }
		});

		let (mut rsend, mut rrecv) = ctrl_conn.open_bi().await.unwrap();
		write_frame(
			&mut rsend,
			&RelayRequest::Route {
				agent: agent_key.clone(),
			},
		)
		.await
		.unwrap();
		rsend.write_all(b"reach across?").await.unwrap();
		let _ = rsend.finish();

		// B doesn't own that agent, so the relay resets the stream as agent-unavailable
		// — B can never see A's machine.
		match with_timeout("cross-tenant route", rrecv.read_to_end(64)).await {
			Err(quinn::ReadToEndError::Read(quinn::ReadError::Reset(code))) => {
				assert_eq!(code, AGENT_UNAVAILABLE.into(), "another tenant's agent is unreachable");
			}
			other => panic!("expected an isolation reset, got {other:?}"),
		}
		// And the agent stays visible within its own tenant A.
		assert!(tenant_a.agent(&agent_key).is_some());
	}

	#[tokio::test]
	async fn admin_channel_provisions_end_to_end() {
		// A full end-to-end admin session: dial, handshake as admin, and provision a
		// tenant over the wire, then confirm the owner secret authenticates a controller.
		let (relay_ep, addr) = relay_server();
		let cfg = ServerConfig::generate();
		let admin_secret = cfg.admin_secret.clone();
		let relay = Relay::new(&cfg, test_config_path());

		// Relay side: accept one connection and serve it (auth + dispatch).
		tokio::spawn({
			let relay = relay.clone();
			let ep = relay_ep.clone();
			async move {
				let incoming = ep.accept().await.unwrap();
				let conn = incoming.accept().unwrap().await.unwrap();
				let authed = authenticate(&relay, &conn).await.unwrap().expect("admin authenticates");
				let Authed::Admin { full, send } = authed else {
					panic!("expected an admin");
				};
				serve_admin(relay, full, conn, send).await.unwrap();
			}
		});

		// Client side: dial, run the admin handshake, and provision.
		let client_ep = tls::client_endpoint(addr).unwrap();
		let conn = client_ep.connect(addr, "libretether.local").unwrap().await.unwrap();
		let id = Identity::generate();
		libretether_protocol::relay::client_handshake(&conn, RelayRole::Admin, &admin_secret, &id)
			.await
			.expect("admin handshake");
		let resp = with_timeout(
			"provision",
			libretether_protocol::relay::admin_request(&conn, &AdminRequest::Provision { name: "team".into() }),
		)
		.await
		.unwrap();
		let AdminResponse::Provisioned(creds) = resp else {
			panic!("expected a provisioned tenant, got {resp:?}");
		};
		// The minted owner secret now resolves to the new tenant on the relay.
		assert_eq!(relay.tenant_by_owner(&creds.owner_secret).unwrap().name, "team");
	}
}
