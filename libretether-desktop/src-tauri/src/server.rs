//! The active controller's connection layer. In direct/Tailscale mode it runs a
//! QUIC server agents dial into; in relay mode it dials the relay and learns
//! about agents through presence events. Both paths share [`enroll_and_register`]
//! and address agents through an [`AgentLink`]. All state lives on the
//! [`ActiveController`] passed in, so each controller is fully isolated.

use std::collections::HashSet;
use std::net::{Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use libretether_common::Backoff;
use libretether_protocol::crypto;
use libretether_protocol::frame::{read_frame, read_frame_capped, write_frame, MAX_CONTROL_FRAME};
use libretether_protocol::relay::{self, RelayEvent, RelayRequest, RelayRole};
use libretether_protocol::{
	tls, Challenge, ControlRequest, ControlResponse, Hello, HelloAck, LogsResult, SessionGrant, StreamOpen,
	DEFAULT_EXEC_TIMEOUT_SECS, MAX_EXEC_TIMEOUT_SECS, PROTOCOL_VERSION,
};
use tauri::Emitter;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::link::AgentLink;
use crate::state::{ActiveController, AppState, ControllerKind, LiveConn};

/// Relay-connection progress, surfaced to the connecting screen.
pub const EVENT_RELAY_LOG: &str = "controller:log";
pub const EVENT_RELAY_CONNECTED: &str = "controller:connected";

/// How long the handshake may take before we drop a connecting peer, so one that
/// connects and then stalls can't tie up a task indefinitely.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the controller waits for the next relay event (presence or heartbeat)
/// before treating the relay as wedged and reconnecting. The relay heartbeats
/// every few seconds, so this is several missed beats — long enough not to trip on
/// a brief hiccup, short enough that a stuck relay doesn't strand every agent as
/// "offline" indefinitely. QUIC keep-alives catch a dead transport; this catches a
/// relay whose routing loop has stalled while its QUIC stack still answers.
const RELAY_READ_TIMEOUT: Duration = Duration::from_secs(20);

/// Per-registration generation source, so a stale connection's teardown only
/// evicts its own live entry and not a newer reconnection's (see `cleanup`).
static LIVE_GEN: AtomicU64 = AtomicU64::new(1);

fn log(msg: &str) {
	crate::logbook::info("controller", msg);
}

fn log_debug(msg: &str) {
	crate::logbook::debug("controller", msg);
}

fn log_warn(msg: &str) {
	crate::logbook::warn("controller", msg);
}

fn log_err(msg: &str) {
	crate::logbook::error("controller", msg);
}

/// Log a relay-connection line and mirror it to the UI's connecting screen.
fn relay_log(state: &AppState, line: &str) {
	log(line);
	if let Some(app) = state.0.app.get() {
		let _ = app.emit(EVENT_RELAY_LOG, line.to_string());
	}
}

// ---------------------------------------------------------------- direct mode

/// Bind the QUIC listener and accept agents forever (direct / Tailscale mode).
pub async fn serve(state: AppState, ctrl: Arc<ActiveController>) {
	let (cert, key) = match ctrl.profile.cert_key_der() {
		Ok(ck) => ck,
		Err(e) => {
			log_err(&format!("invalid controller certificate: {e}"));
			return;
		}
	};
	let port = ctrl.profile.kind.listen_port();

	// Bind on all interfaces rather than just the tailnet IP: pinning to the tailnet
	// address is fragile (it fails if Tailscale isn't up yet at start, leaving the
	// controller unable to listen at all), and the wider bind is safe because every
	// agent is authenticated end-to-end with Ed25519 — an attacker who reaches the
	// port but can't complete the mutual handshake is rejected before any command.
	//
	// Bind the dual-stack `[::]` wildcard (not `0.0.0.0`, which is IPv4-only) so an
	// agent can dial the controller over IPv6 as well as IPv4 — otherwise a Direct
	// controller reachable only at its IPv6 address is silently unreachable. See
	// `tls::server_endpoint`.
	let endpoint = match tls::server_endpoint(cert, key, SocketAddr::from((Ipv6Addr::UNSPECIFIED, port))) {
		Ok(ep) => ep,
		Err(e) => {
			log_err(&format!("could not listen on udp/{port}: {e}"));
			return;
		}
	};
	log(&format!("[{}] listening for agents on udp/{port}", ctrl.profile.name));

	while let Some(incoming) = endpoint.accept().await {
		log_debug(&format!("connection received from {}", incoming.remote_address()));
		let state = state.clone();
		let ctrl = ctrl.clone();
		tauri::async_runtime::spawn(async move {
			if let Err(e) = handle_direct(state, ctrl, incoming).await {
				log_err(&format!("connection error: {e}"));
			}
		});
	}
}

async fn handle_direct(state: AppState, ctrl: Arc<ActiveController>, incoming: quinn::Incoming) -> AppResult<()> {
	let remote = incoming.remote_address();
	let conn = incoming
		.accept()
		.map_err(|e| AppError::msg(format!("accept: {e}")))?
		.await
		.map_err(|e| AppError::msg(format!("handshake: {e}")))?;
	log_debug(&format!(
		"quic connection established with {remote}; starting handshake"
	));

	let link = AgentLink::direct(conn.clone());
	if let Some((id, generation)) = enroll_and_register(&state, &ctrl, link).await? {
		conn.closed().await;
		cleanup(&state, &ctrl, id, Some(generation));
		log(&format!("agent {id} disconnected"));
	}
	Ok(())
}

// ---------------------------------------------------------------- relay mode

/// Dial the relay and track agents through it (relay mode), reconnecting on drop.
pub async fn serve_relay(state: AppState, ctrl: Arc<ActiveController>) {
	let (relay_addr, owner_secret) = match &ctrl.profile.kind {
		ControllerKind::Relay {
			address, owner_secret, ..
		} => (address.clone(), owner_secret.clone()),
		_ => {
			log_err("relay serve started for a non-relay controller");
			return;
		}
	};
	let mut backoff = Backoff::new(5);
	loop {
		match relay_session(&state, &ctrl, &relay_addr, &owner_secret).await {
			Ok(()) => backoff.reset(),
			Err(e) => relay_log(&state, &format!("relay error: {e:#}")),
		}
		// The relay is gone, so every agent is unreachable and the connection a
		// `relay_logs` fetch would use is dead.
		ctrl.live.lock().unwrap().clear();
		*ctrl.relay_conn.lock().unwrap() = None;
		state.notify_changed();
		let wait = backoff.next_delay();
		relay_log(&state, &format!("reconnecting to relay in {}s", wait.as_secs()));
		tokio::time::sleep(wait).await;
	}
}

async fn relay_session(
	state: &AppState,
	ctrl: &Arc<ActiveController>,
	relay_addr: &str,
	owner_secret: &str,
) -> AppResult<()> {
	let addr = tls::resolve(relay_addr)
		.await
		.map_err(|e| AppError::msg(format!("resolving {relay_addr}: {e}")))?;
	relay_log(state, &format!("dialing relay at {addr}"));
	let endpoint = tls::client_endpoint(addr).map_err(|e| AppError::msg(format!("bind client: {e}")))?;
	let conn = endpoint
		.connect(addr, "libretether.local")
		.map_err(|e| AppError::msg(e.to_string()))?
		.await
		.map_err(|e| AppError::msg(format!("relay handshake: {e}")))?;

	// Shared client side of the relay handshake (secret + key-ownership proof). We
	// keep `recv` to read presence events; `_send` stays open for the connection's
	// life. Our routing key is derived from the identity inside the handshake.
	let (_send, mut recv) =
		relay::client_handshake(&conn, RelayRole::Controller, owner_secret, &ctrl.profile.identity()?).await?;
	relay_log(state, "connected to relay; awaiting agents");
	// Publish the live connection so commands can reach the relay itself (e.g. the
	// Logs page fetching the relay's server log). Cleared when the session ends.
	*ctrl.relay_conn.lock().unwrap() = Some(conn.clone());
	if let Some(app) = state.0.app.get() {
		let _ = app.emit(EVENT_RELAY_CONNECTED, ());
	}

	// Continuously pull the relay's own server log in the background and fold it into
	// the controller's logbook, so it's captured and persisted regardless of whether
	// the Logs page is open. Tied to this session: the guard aborts it on any exit.
	let _relay_log_poller = AbortOnDrop(tauri::async_runtime::spawn(relay_log_poll(ctrl.clone(), conn.clone())));

	// Public keys with an enrollment handshake in flight, so a duplicate presence
	// event (relay flap / replay) doesn't start a second concurrent handshake for
	// the same agent and race two live-map inserts.
	let enrolling: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

	loop {
		// Bound the wait on the next event: the relay heartbeats periodically, so a
		// gap longer than RELAY_READ_TIMEOUT means the relay's routing loop is wedged
		// (its QUIC keep-alives may still be answering) — drop and reconnect rather
		// than leave every agent showing offline forever.
		let event: RelayEvent =
			match tokio::time::timeout(RELAY_READ_TIMEOUT, read_frame_capped(&mut recv, MAX_CONTROL_FRAME)).await {
				Ok(res) => res?,
				Err(_) => return Err(AppError::msg("relay went quiet (no heartbeat) — reconnecting")),
			};
		match event {
			// Liveness only — proves the relay is still servicing us.
			RelayEvent::Heartbeat => {}
			RelayEvent::AgentOnline { public_key } => {
				log_debug(&format!(
					"relay reports agent online ({}…)",
					public_key.chars().take(8).collect::<String>()
				));
				// Skip if an enrollment for this agent is already running (de-dup a
				// presence replay); the in-flight handshake will register it.
				if !enrolling.lock().unwrap().insert(public_key.clone()) {
					continue;
				}
				let state = state.clone();
				let ctrl = ctrl.clone();
				let enrolling = enrolling.clone();
				let link = AgentLink::relay(conn.clone(), public_key.clone());
				tauri::async_runtime::spawn(async move {
					if let Err(e) = enroll_and_register(&state, &ctrl, link).await {
						log_err(&format!("enroll via relay failed: {e}"));
					}
					enrolling.lock().unwrap().remove(&public_key);
				});
			}
			RelayEvent::AgentOffline { public_key } => {
				log_debug(&format!(
					"relay reports agent offline ({}…)",
					public_key.chars().take(8).collect::<String>()
				));
				let id = ctrl.store.lock().unwrap().id_for_pubkey(&public_key);
				if let Some(id) = id {
					// The relay is authoritative for relay-mode presence, so remove
					// unconditionally (the relay only reports offline once the agent's
					// current connection has actually dropped).
					cleanup(state, ctrl, id, None);
				}
			}
		}
	}
}

/// Fetch the relay server's own log lines over the live relay connection (relay
/// mode only). Opens a fresh stream, leads with [`RelayRequest::FetchLogs`] so the
/// relay answers it itself rather than routing it to an agent, and reads the
/// [`LogsResult`] back. With `after_seq` set the relay returns only lines recorded
/// since that cursor (incremental poll); `None` fetches the full retained buffer.
/// Uses the wide `read_frame` cap because a full log snapshot can exceed the 1 MiB
/// control cap.
pub async fn fetch_relay_logs(conn: &quinn::Connection, after_seq: Option<u64>) -> AppResult<LogsResult> {
	let (mut send, mut recv) = conn
		.open_bi()
		.await
		.map_err(|e| AppError::msg(format!("open relay stream: {e}")))?;
	write_frame(&mut send, &RelayRequest::FetchLogs { after_seq }).await?;
	let _ = send.finish();
	Ok(read_frame::<_, LogsResult>(&mut recv).await?)
}

/// How often the controller polls the relay for new server-log lines and folds them
/// into its own logbook (so they persist and show on the Logs page even when it's
/// not open). Low-volume and cheap (one short stream per poll); a few seconds keeps
/// the relay's log feeling live without being chatty.
const RELAY_LOG_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Aborts a spawned task when dropped, so a child task tied to a scope (here, the
/// relay-log poller bound to one relay session) can't outlive it.
struct AbortOnDrop(tauri::async_runtime::JoinHandle<()>);

impl Drop for AbortOnDrop {
	fn drop(&mut self) {
		self.0.abort();
	}
}

/// Poll the relay for new log lines on `conn` and fold them into the controller's
/// logbook (source `"relay"`), so relay activity is captured continuously in the
/// background — persisted in the ring and streamed live to the Logs page — instead
/// of only when an operator manually fetches it. Runs until the connection drops
/// (then the reconnect spawns a fresh poller) or the task is aborted at session end.
async fn relay_log_poll(ctrl: Arc<ActiveController>, conn: quinn::Connection) {
	let mut ticker = tokio::time::interval(RELAY_LOG_POLL_INTERVAL);
	loop {
		ticker.tick().await;
		// `u64::MAX` is the "nothing fetched yet" sentinel → fetch the full buffer.
		let cursor = ctrl.relay_log_seq.load(Ordering::Relaxed);
		let after = (cursor != u64::MAX).then_some(cursor);
		match fetch_relay_logs(&conn, after).await {
			Ok(result) => {
				// Re-anchor the relay's timestamps to OUR clock (it may be in another
				// timezone or skewed), exactly as the agent-log path does.
				let offset = libretether_common::now_secs() as i64 - result.now_secs as i64;
				for line in &result.lines {
					let ts = (line.ts_secs as i64 + offset).max(0) as u64;
					crate::logbook::record_at(ts, line.level, "relay", &line.message);
				}
				ctrl.relay_log_seq.store(result.next_seq, Ordering::Relaxed);
			}
			Err(_) => {
				// The connection is gone — stop; the outer reconnect loop will bring up
				// a new session (and a fresh poller). A transient hiccup on a still-live
				// connection just retries on the next tick.
				if conn.close_reason().is_some() {
					return;
				}
			}
		}
	}
}

// ---------------------------------------------------------------- shared

/// Run the Ed25519 handshake over `link`, enroll/identify the agent, register it
/// in the live map and pull an initial status. Returns the client id and its
/// registration generation on success.
async fn enroll_and_register(
	state: &AppState,
	ctrl: &ActiveController,
	link: AgentLink,
) -> AppResult<Option<(Uuid, u64)>> {
	let (mut send, mut recv) = link.open_bi().await?;
	write_frame(&mut send, &StreamOpen::Handshake).await?;
	log_debug("handshake: opened stream, sending challenge");
	let nonce = crypto::random_nonce_b64();
	write_frame(
		&mut send,
		&Challenge {
			protocol: PROTOCOL_VERSION,
			nonce: nonce.clone(),
			controller_key: ctrl.profile.public_key(),
		},
	)
	.await?;

	// Read the agent's Hello under a timeout (handshake frames are small), so a
	// peer that connects and then stalls can't tie up this task.
	let hello: Hello =
		match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame_capped(&mut recv, MAX_CONTROL_FRAME)).await {
			Ok(res) => res?,
			Err(_) => return Ok(None),
		};
	log_debug(&format!(
		"handshake: received hello from {} ({})",
		hello.host.hostname, hello.host.os
	));
	if hello.protocol != PROTOCOL_VERSION {
		reject(&mut send, "protocol version mismatch").await;
		return Ok(None);
	}
	if !crypto::verify_b64(&hello.public_key, nonce.as_bytes(), &hello.signature) {
		reject(&mut send, "signature verification failed").await;
		return Ok(None);
	}

	// Enroll/identify the agent and persist the token-burn / last-seen update (the
	// write happens off the store lock; see `mutate_store_warn`).
	let client_id = ctrl.mutate_store_warn("enrollment", |store| {
		let id = store.authenticate(hello.enrollment_token.as_deref(), &hello.public_key);
		(id, id.is_some())
	});
	let Some(client_id) = client_id else {
		reject(&mut send, "unknown agent (no matching enrollment token or key)").await;
		return Ok(None);
	};

	// Authenticate ourselves to the agent in turn: sign its nonce with our
	// identity key, which the agent checks against the key it pinned at enrollment.
	let controller_sig = ctrl.profile.identity()?.sign_b64(hello.agent_nonce.as_bytes());
	write_frame(
		&mut send,
		&HelloAck {
			accepted: true,
			reason: None,
			client_id: Some(client_id.to_string()),
			controller_sig,
		},
	)
	.await?;
	let _ = send.finish();

	// Receive the capability token the agent issues for this connection and bind
	// it to the link, so every control/session/tunnel stream we open is stamped
	// with it (the agent rejects streams that aren't).
	let grant: SessionGrant =
		match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame_capped(&mut recv, MAX_CONTROL_FRAME)).await {
			Ok(res) => res?,
			Err(_) => return Ok(None),
		};
	let link = link.with_token(grant.token);
	log_debug("handshake: capability token received and bound to the link");

	log(&format!(
		"agent {client_id} connected — {} ({})",
		hello.host.hostname, hello.host.os
	));

	let generation = LIVE_GEN.fetch_add(1, Ordering::Relaxed);
	ctrl.live.lock().unwrap().insert(
		client_id,
		LiveConn {
			link: link.clone(),
			status: None,
			generation,
		},
	);
	state.notify_changed();

	log_debug(&format!("requesting initial status from agent {client_id}"));
	if let Ok(ControlResponse::Status(status)) = control_request(&link, &ControlRequest::Status).await {
		if let Some(live) = ctrl.live.lock().unwrap().get_mut(&client_id) {
			live.status = Some(status);
		}
		state.notify_changed();
	}
	Ok(Some((client_id, generation)))
}

/// Drop a client's live entry and mark it last-seen. `generation` guards the
/// direct-mode teardown: a stale connection only evicts the entry it registered,
/// never a newer reconnection's. Relay mode passes `None` (the relay is
/// authoritative there).
fn cleanup(state: &AppState, ctrl: &ActiveController, id: Uuid, generation: Option<u64>) {
	{
		let mut live = ctrl.live.lock().unwrap();
		let is_current = match generation {
			Some(g) => live.get(&id).map(|c| c.generation) == Some(g),
			None => true,
		};
		if is_current {
			live.remove(&id);
		} else {
			// A newer connection replaced us — leave it in place.
			return;
		}
	}
	log_debug(&format!("agent {id} went offline; tearing down its tunnels"));
	// Drop any relay tunnels for this client: they hold the now-defunct
	// connection's capability token, so a reconnect must rebuild them afresh.
	crate::tunnel::close_for(ctrl, id);
	ctrl.mutate_store_warn("last-seen", |store| ((), store.touch_seen(id)));
	state.notify_changed();
}

async fn reject(send: &mut quinn::SendStream, reason: &str) {
	let _ = write_frame(
		send,
		&HelloAck {
			accepted: false,
			reason: Some(reason.to_string()),
			client_id: None,
			controller_sig: String::new(),
		},
	)
	.await;
	let _ = send.finish();
	log_warn(&format!("rejected agent: {reason}"));
}

/// Open a one-shot control stream to an agent, send `req`, and read the response.
/// Bounded by a per-request timeout: an agent that accepts the stream but never
/// replies (compromised, deadlocked, or just buggy) must not hang the caller — and
/// the UI command behind it — indefinitely, since QUIC keep-alives would otherwise
/// hold the connection open forever.
pub async fn control_request(link: &AgentLink, req: &ControlRequest) -> AppResult<ControlResponse> {
	match tokio::time::timeout(request_timeout(req), control_request_inner(link, req)).await {
		Ok(res) => res,
		Err(_) => Err(AppError::Timeout),
	}
}

/// How long a control request may take before we give up. `Exec` waits for the
/// agent's own (bounded) execution budget plus headroom for spawn + transfer;
/// every other request is a short fixed cap.
fn request_timeout(req: &ControlRequest) -> Duration {
	match req {
		ControlRequest::Exec { timeout_secs, .. } => {
			let secs = timeout_secs
				.unwrap_or(DEFAULT_EXEC_TIMEOUT_SECS)
				.clamp(1, MAX_EXEC_TIMEOUT_SECS);
			Duration::from_secs(secs + 15)
		}
		_ => Duration::from_secs(30),
	}
}

async fn control_request_inner(link: &AgentLink, req: &ControlRequest) -> AppResult<ControlResponse> {
	let (mut send, mut recv) = link.open_authenticated(StreamOpen::Control).await?;
	write_frame(&mut send, req).await?;
	let _ = send.finish();
	// A `Screenshot` response carries a full-screen PNG, so this read intentionally
	// uses the wide `MAX_FRAME` cap (not the 1 MiB control cap). The exposure is one
	// such allocation per outstanding request from an already-authenticated agent,
	// and `control_request` wraps every call in a per-request timeout.
	Ok(read_frame::<_, ControlResponse>(&mut recv).await?)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::registry::{ClientOs, ClientStore};
	use crate::state::ControllerProfile;
	// Direct/Tailscale serving now binds via `tls::server_endpoint`; the loopback test
	// harness builds bare endpoints itself, so `Endpoint` is only used under test.
	use libretether_protocol::crypto::Identity;
	use libretether_protocol::{AgentStatus, HostInfo, StreamAuth};
	use quinn::Endpoint;
	use std::net::Ipv4Addr;

	fn temp_dir() -> std::path::PathBuf {
		use std::sync::atomic::{AtomicU32, Ordering};
		static N: AtomicU32 = AtomicU32::new(0);
		let p = std::env::temp_dir().join(format!(
			"lt-server-{}-{}",
			std::process::id(),
			N.fetch_add(1, Ordering::Relaxed)
		));
		let _ = std::fs::remove_dir_all(&p);
		p
	}

	/// An `AppState` + a fresh direct-mode `ActiveController` holding one
	/// not-yet-enrolled client. Returns the client's one-time enrollment token.
	fn setup() -> (AppState, ActiveController, String) {
		let dir = temp_dir();
		let state = AppState::init(dir.clone()).unwrap();
		let profile = ControllerProfile::new(
			"test".into(),
			ControllerKind::Direct {
				advertise_addr: None,
				listen_port: 0,
			},
		);
		let mut store = ClientStore::load(dir.join("clients.json")).unwrap();
		let token = store.create("box".into(), ClientOs::Linux).enrollment_token.unwrap();
		let ctrl = ActiveController::new(profile, store);
		(state, ctrl, token)
	}

	/// A connected QUIC pair: the controller is the server (it opens streams), the
	/// agent is the client (it accepts). Endpoints are returned so the test keeps
	/// them — and the connections — alive.
	async fn loopback() -> (Endpoint, quinn::Connection, Endpoint, quinn::Connection) {
		tls::install_crypto_provider();
		let (cert, key) = tls::self_signed();
		let server_ep = Endpoint::server(tls::server_config(cert, key), (Ipv4Addr::LOCALHOST, 0).into()).unwrap();
		let addr = server_ep.local_addr().unwrap();
		let client_ep = tls::client_endpoint(addr).unwrap();
		let accept = {
			let ep = server_ep.clone();
			tokio::spawn(async move { ep.accept().await.unwrap().accept().unwrap().await.unwrap() })
		};
		let client_conn = client_ep.connect(addr, "libretether.local").unwrap().await.unwrap();
		let server_conn = accept.await.unwrap();
		(server_ep, server_conn, client_ep, client_conn)
	}

	fn host() -> HostInfo {
		HostInfo {
			hostname: "box".into(),
			os: "linux".into(),
			arch: "x86_64".into(),
			username: "u".into(),
		}
	}

	#[tokio::test]
	async fn enrolls_burns_token_authenticates_both_ways_and_grants_a_session() {
		let (state, ctrl, token) = setup();
		let ctrl_pub = ctrl.profile.public_key();
		let (_sep, server, _cep, client) = loopback().await;
		let agent = Identity::generate();
		let agent_pub = agent.public_b64();

		// A faithful agent: signs the challenge, verifies the controller, issues a
		// capability token, then serves the controller's initial Status request.
		let agent_task = tokio::spawn(async move {
			let (mut a_send, mut a_recv) = client.accept_bi().await.unwrap();
			let open: StreamOpen = read_frame_capped(&mut a_recv, MAX_CONTROL_FRAME).await.unwrap();
			assert!(matches!(open, StreamOpen::Handshake));
			let challenge: Challenge = read_frame_capped(&mut a_recv, MAX_CONTROL_FRAME).await.unwrap();
			assert_eq!(challenge.controller_key, ctrl_pub, "controller presents its pinned key");
			let agent_nonce = crypto::random_nonce_b64();
			let hello = Hello {
				protocol: PROTOCOL_VERSION,
				enrollment_token: Some(token),
				public_key: agent.public_b64(),
				signature: agent.sign_b64(challenge.nonce.as_bytes()),
				agent_nonce: agent_nonce.clone(),
				host: host(),
				agent_version: "test".into(),
			};
			write_frame(&mut a_send, &hello).await.unwrap();
			let ack: HelloAck = read_frame_capped(&mut a_recv, MAX_CONTROL_FRAME).await.unwrap();
			assert!(ack.accepted, "controller should accept: {:?}", ack.reason);
			// The controller authenticated itself (mutual auth): signature over our
			// nonce verifies against the key it presented.
			assert!(crypto::verify_b64(
				&challenge.controller_key,
				agent_nonce.as_bytes(),
				&ack.controller_sig
			));
			let cap = crypto::random_nonce_b64();
			write_frame(&mut a_send, &SessionGrant { token: cap.clone() })
				.await
				.unwrap();
			let _ = a_send.finish();

			// Initial Status: must arrive stamped with the capability token.
			let (mut s_send, mut s_recv) = client.accept_bi().await.unwrap();
			let open2: StreamOpen = read_frame_capped(&mut s_recv, MAX_CONTROL_FRAME).await.unwrap();
			assert!(matches!(open2, StreamOpen::Control));
			let auth: StreamAuth = read_frame_capped(&mut s_recv, MAX_CONTROL_FRAME).await.unwrap();
			assert_eq!(auth.token, cap, "control stream carries the issued token");
			let req: ControlRequest = read_frame_capped(&mut s_recv, MAX_CONTROL_FRAME).await.unwrap();
			assert!(matches!(req, ControlRequest::Status));
			let status = AgentStatus {
				host: host(),
				agent_version: "test".into(),
				uptime_secs: 1,
				started_at: 0,
				boot_time_secs: None,
				displays: 1,
				tailscale_ip: None,
			};
			write_frame(&mut s_send, &ControlResponse::Status(status))
				.await
				.unwrap();
			let _ = s_send.finish();
			client
		});

		let res = enroll_and_register(&state, &ctrl, AgentLink::direct(server.clone()))
			.await
			.unwrap();
		let (client_id, _gen) = res.expect("handshake should succeed");
		let _client = agent_task.await.unwrap();

		// Registered live, with the initial status captured.
		{
			let live = ctrl.live.lock().unwrap();
			let conn = live.get(&client_id).expect("registered in the live map");
			assert!(conn.status.is_some(), "initial status recorded");
		}
		// Persisted: key bound, one-time token burned.
		let store = ctrl.store.lock().unwrap();
		let c = store.get(client_id).unwrap();
		assert!(c.enrolled);
		assert_eq!(c.public_key.as_deref(), Some(agent_pub.as_str()));
		assert!(c.enrollment_token.is_none(), "the one-time token is burned");
	}

	/// Drive an agent that fails the handshake in one specific way, and return the
	/// rejection ack it receives. `mangle` mutates an otherwise-valid `Hello`.
	async fn run_rejected_agent(
		client: quinn::Connection,
		agent: Identity,
		token: Option<String>,
		mangle: impl FnOnce(&mut Hello) + Send + 'static,
	) -> HelloAck {
		let (mut a_send, mut a_recv) = client.accept_bi().await.unwrap();
		let _open: StreamOpen = read_frame_capped(&mut a_recv, MAX_CONTROL_FRAME).await.unwrap();
		let challenge: Challenge = read_frame_capped(&mut a_recv, MAX_CONTROL_FRAME).await.unwrap();
		let mut hello = Hello {
			protocol: PROTOCOL_VERSION,
			enrollment_token: token,
			public_key: agent.public_b64(),
			signature: agent.sign_b64(challenge.nonce.as_bytes()),
			agent_nonce: crypto::random_nonce_b64(),
			host: host(),
			agent_version: "test".into(),
		};
		mangle(&mut hello);
		write_frame(&mut a_send, &hello).await.unwrap();
		read_frame_capped(&mut a_recv, MAX_CONTROL_FRAME).await.unwrap()
	}

	#[tokio::test]
	async fn rejects_a_protocol_version_mismatch() {
		let (state, ctrl, token) = setup();
		let (_sep, server, _cep, client) = loopback().await;
		let agent = Identity::generate();
		let agent_task = tokio::spawn(run_rejected_agent(client, agent, Some(token), |h| {
			h.protocol = PROTOCOL_VERSION + 1
		}));

		let res = enroll_and_register(&state, &ctrl, AgentLink::direct(server.clone()))
			.await
			.unwrap();
		assert!(res.is_none(), "version mismatch must be rejected");
		assert!(!agent_task.await.unwrap().accepted);
	}

	#[tokio::test]
	async fn rejects_a_bad_agent_signature() {
		let (state, ctrl, token) = setup();
		let (_sep, server, _cep, client) = loopback().await;
		let agent = Identity::generate();
		// Replace the signature with one that won't verify against the public key.
		let agent_task = tokio::spawn(run_rejected_agent(client, agent, Some(token), |h| {
			h.signature = Identity::generate().sign_b64(b"unrelated");
		}));

		let res = enroll_and_register(&state, &ctrl, AgentLink::direct(server.clone()))
			.await
			.unwrap();
		assert!(res.is_none(), "an unverifiable signature must be rejected");
		assert!(!agent_task.await.unwrap().accepted);
	}

	#[tokio::test]
	async fn rejects_an_unknown_enrollment_token() {
		let (state, ctrl, _real_token) = setup();
		let (_sep, server, _cep, client) = loopback().await;
		let agent = Identity::generate();
		// Valid self-signature, but a token that matches no client and a key that
		// isn't bound to one — the agent is unknown.
		let agent_task = tokio::spawn(run_rejected_agent(
			client,
			agent,
			Some("not-a-real-token".into()),
			|_| {},
		));

		let res = enroll_and_register(&state, &ctrl, AgentLink::direct(server.clone()))
			.await
			.unwrap();
		assert!(res.is_none(), "an unknown agent must be rejected");
		assert!(!agent_task.await.unwrap().accepted);
	}

	#[tokio::test]
	async fn cleanup_only_evicts_its_own_generation() {
		let (state, ctrl, _token) = setup();
		let id = ctrl.store.lock().unwrap().list()[0].id;
		// A live entry needs a connection for its link; loopback gives a real one.
		let (_sep, server, _cep, _client) = loopback().await;
		ctrl.live.lock().unwrap().insert(
			id,
			LiveConn {
				link: AgentLink::direct(server.clone()),
				status: None,
				generation: 7,
			},
		);

		// A stale teardown (an older generation, e.g. a previous connection that just
		// noticed it dropped) must NOT evict the newer live entry.
		cleanup(&state, &ctrl, id, Some(6));
		assert!(
			ctrl.is_online(id),
			"stale-generation cleanup must leave the newer entry in place"
		);

		// The matching generation is the real owner and does evict.
		cleanup(&state, &ctrl, id, Some(7));
		assert!(!ctrl.is_online(id));

		// Relay-mode cleanup (generation `None`) is unconditional.
		ctrl.live.lock().unwrap().insert(
			id,
			LiveConn {
				link: AgentLink::direct(server.clone()),
				status: None,
				generation: 9,
			},
		);
		cleanup(&state, &ctrl, id, None);
		assert!(!ctrl.is_online(id), "relay-authoritative cleanup always evicts");
	}
}
