//! A built-in SSH server the agent runs in-process, so a controller can get a
//! shell on any client even when the OS has no SSH server installed (notably a
//! stock Windows box, where enabling OpenSSH Server is slow and needs admin).
//!
//! It binds **loopback only** and is reached through the same authenticated tunnel
//! the controller already uses for RDP/SSH, so the network path is trusted; on top
//! of that it accepts a single ephemeral public key (the controller holds the
//! matching private key). The shell runs as the user the agent runs as — we never
//! perform an OS logon, which is exactly why this works unprivileged where a
//! bundled `sshd` would not.
//!
//! Started lazily on the first [`crate::handlers`] `EnableSsh`; one instance serves
//! every connection for the life of the process.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use rand::RngCore;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::ssh_key::public::KeyData;
use russh::keys::ssh_key::{LineEnding, PrivateKey, PublicKey};
use russh::server::{Auth, Config, Handler, Msg, Session};
use russh::{Channel, ChannelId, Pty};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, OnceCell};

use crate::net::log;

/// How to reach the running embedded server: its loopback port and the OpenSSH
/// private key authorized to log in.
#[derive(Clone)]
pub struct Embedded {
	pub port: u16,
	pub private_key_openssh: String,
}

static EMBEDDED: OnceCell<Embedded> = OnceCell::const_new();

/// Ensure the embedded SSH server is running, returning how to reach it. Starts it
/// (binding `127.0.0.1:0`) on the first call; later calls return the same server.
pub async fn ensure() -> Result<Embedded, String> {
	match EMBEDDED.get_or_try_init(start).await {
		Ok(embedded) => Ok(embedded.clone()),
		Err(e) => Err(e),
	}
}

fn random_seed() -> [u8; 32] {
	let mut seed = [0u8; 32];
	rand::rng().fill_bytes(&mut seed);
	seed
}

/// A fresh Ed25519 key from 32 random seed bytes — avoids threading an RNG through
/// `ssh-key`'s generic `random` (whose `rand_core` version is awkward to match).
fn ed25519_key() -> PrivateKey {
	PrivateKey::from(Ed25519Keypair::from_seed(&random_seed()))
}

async fn start() -> Result<Embedded, String> {
	// A host key for the server, and a separate keypair the controller uses to log
	// in (we keep its public half as the sole authorized key and hand the private
	// half to the controller).
	let host_key = ed25519_key();
	let login_key = ed25519_key();
	let authorized = login_key.public_key().key_data().clone();
	let private_key_openssh = login_key
		.to_openssh(LineEnding::LF)
		.map_err(|e| format!("encoding ssh key: {e}"))?
		.to_string();

	let listener = TcpListener::bind(("127.0.0.1", 0))
		.await
		.map_err(|e| format!("binding ssh server: {e}"))?;
	let port = listener
		.local_addr()
		.map_err(|e| format!("ssh server addr: {e}"))?
		.port();

	let config = Arc::new(Config {
		// A connection that authenticates and then idles is fine for an interactive
		// shell; this just bounds one that stalls mid-handshake.
		inactivity_timeout: Some(Duration::from_secs(3600)),
		auth_rejection_time: Duration::from_secs(2),
		keys: vec![host_key],
		..Default::default()
	});

	tokio::spawn(accept_loop(listener, config, authorized));
	log(&format!("embedded ssh server listening on 127.0.0.1:{port}"));
	Ok(Embedded {
		port,
		private_key_openssh,
	})
}

async fn accept_loop(listener: TcpListener, config: Arc<Config>, authorized: KeyData) {
	loop {
		let Ok((stream, _)) = listener.accept().await else {
			continue;
		};
		let config = config.clone();
		let handler = SessionHandler::new(authorized.clone());
		tokio::spawn(async move {
			if let Ok(session) = russh::server::run_stream(config, stream, handler).await {
				// Drive the connection to completion; errors just mean it closed.
				let _ = session.await;
			}
		});
	}
}

/// Per-channel running shell: where to send the client's keystrokes, the PTY
/// master (for window resizes), and a killer to stop the shell on close.
struct ChannelShell {
	input_tx: mpsc::UnboundedSender<Vec<u8>>,
	master: Box<dyn MasterPty + Send>,
	killer: Box<dyn ChildKiller + Send + Sync>,
}

/// One client connection's handler. Authorizes the single ephemeral key, then
/// bridges a PTY-backed shell to the SSH channel.
struct SessionHandler {
	authorized: KeyData,
	/// PTY size requested per channel before its shell starts (`pty-req` precedes `shell`).
	pending_pty: HashMap<ChannelId, (u16, u16, String)>,
	shells: HashMap<ChannelId, ChannelShell>,
}

impl SessionHandler {
	fn new(authorized: KeyData) -> Self {
		Self {
			authorized,
			pending_pty: HashMap::new(),
			shells: HashMap::new(),
		}
	}

	/// Open a PTY, spawn the shell, and wire the PTY to the SSH `channel`: client
	/// input → PTY stdin, PTY output → channel, child exit → exit-status + close.
	fn spawn_shell(
		&self,
		channel: ChannelId,
		cols: u16,
		rows: u16,
		term: &str,
		session: &Session,
	) -> Result<ChannelShell, String> {
		let pair = native_pty_system()
			.openpty(PtySize {
				rows,
				cols,
				pixel_width: 0,
				pixel_height: 0,
			})
			.map_err(|e| e.to_string())?;
		let child = pair
			.slave
			.spawn_command(shell_command(term))
			.map_err(|e| e.to_string())?;
		// Close our copy of the slave so the PTY reports EOF once the child exits.
		drop(pair.slave);
		let killer = child.clone_killer();

		let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
		let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
		let handle = session.handle();

		// Client keystrokes → PTY stdin (blocking writer on its own thread).
		let (input_tx, input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
		std::thread::spawn(move || pump_input(input_rx, writer));

		// PTY output → channel, and the child's exit code, joined so the exit status
		// is sent only after all output has been forwarded.
		let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
		let (exit_tx, exit_rx) = oneshot::channel::<u32>();
		std::thread::spawn(move || pump_output(reader, out_tx));
		std::thread::spawn(move || {
			let mut child = child;
			let code = child.wait().map(|s| s.exit_code()).unwrap_or(1);
			let _ = exit_tx.send(code);
		});
		tokio::spawn(forward_output(handle, channel, out_rx, exit_rx));

		Ok(ChannelShell {
			input_tx,
			master: pair.master,
			killer,
		})
	}
}

impl Drop for SessionHandler {
	fn drop(&mut self) {
		// Don't leave shells running after the SSH connection is gone.
		for shell in self.shells.values_mut() {
			let _ = shell.killer.kill();
		}
	}
}

impl Handler for SessionHandler {
	type Error = russh::Error;

	async fn auth_publickey(&mut self, _user: &str, public_key: &PublicKey) -> Result<Auth, Self::Error> {
		// Accept only the one ephemeral key we handed the controller; compare the key
		// data (not the whole `PublicKey`, whose comment may differ).
		if public_key.key_data() == &self.authorized {
			Ok(Auth::Accept)
		} else {
			Ok(Auth::reject())
		}
	}

	async fn channel_open_session(
		&mut self,
		_channel: Channel<Msg>,
		_session: &mut Session,
	) -> Result<bool, Self::Error> {
		Ok(true)
	}

	async fn pty_request(
		&mut self,
		channel: ChannelId,
		term: &str,
		col_width: u32,
		row_height: u32,
		_pix_width: u32,
		_pix_height: u32,
		_modes: &[(Pty, u32)],
		session: &mut Session,
	) -> Result<(), Self::Error> {
		self.pending_pty
			.insert(channel, (col_width as u16, row_height as u16, term.to_string()));
		session.channel_success(channel)?;
		Ok(())
	}

	async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
		let (cols, rows, term) = self
			.pending_pty
			.remove(&channel)
			.unwrap_or((80, 24, "xterm-256color".to_string()));
		match self.spawn_shell(channel, cols, rows, &term, session) {
			Ok(shell) => {
				self.shells.insert(channel, shell);
				session.channel_success(channel)?;
			}
			Err(e) => {
				log(&format!("embedded ssh: shell spawn failed: {e}"));
				session.channel_failure(channel)?;
			}
		}
		Ok(())
	}

	async fn data(&mut self, channel: ChannelId, data: &[u8], _session: &mut Session) -> Result<(), Self::Error> {
		if let Some(shell) = self.shells.get(&channel) {
			let _ = shell.input_tx.send(data.to_vec());
		}
		Ok(())
	}

	async fn window_change_request(
		&mut self,
		channel: ChannelId,
		col_width: u32,
		row_height: u32,
		_pix_width: u32,
		_pix_height: u32,
		_session: &mut Session,
	) -> Result<(), Self::Error> {
		if let Some(shell) = self.shells.get(&channel) {
			let _ = shell.master.resize(PtySize {
				rows: row_height as u16,
				cols: col_width as u16,
				pixel_width: 0,
				pixel_height: 0,
			});
		}
		Ok(())
	}

	async fn channel_close(&mut self, channel: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
		if let Some(mut shell) = self.shells.remove(&channel) {
			let _ = shell.killer.kill();
		}
		Ok(())
	}
}

/// The shell to run: the user's default shell on Unix, PowerShell on Windows.
/// `CommandBuilder::new` already inherits the agent's environment.
fn shell_command(term: &str) -> CommandBuilder {
	#[cfg(windows)]
	let mut cmd = CommandBuilder::new("powershell.exe");
	#[cfg(not(windows))]
	let mut cmd = CommandBuilder::new_default_prog();
	cmd.env("TERM", if term.is_empty() { "xterm-256color" } else { term });
	if let Some(home) = dirs::home_dir() {
		cmd.cwd(home);
	}
	cmd
}

/// Drain client input to the PTY's stdin. Runs on a blocking thread (the PTY
/// writer is synchronous); ends when the channel's sender drops.
fn pump_input(mut input_rx: mpsc::UnboundedReceiver<Vec<u8>>, mut writer: Box<dyn Write + Send>) {
	while let Some(data) = input_rx.blocking_recv() {
		if writer.write_all(&data).is_err() {
			break;
		}
		let _ = writer.flush();
	}
}

/// Read the PTY's output on a blocking thread, forwarding chunks to `out_tx` until
/// EOF (the child exited and the PTY drained).
fn pump_output(mut reader: Box<dyn Read + Send>, out_tx: mpsc::UnboundedSender<Vec<u8>>) {
	let mut buf = [0u8; 8192];
	loop {
		match reader.read(&mut buf) {
			Ok(0) | Err(_) => break,
			Ok(n) => {
				if out_tx.send(buf[..n].to_vec()).is_err() {
					break;
				}
			}
		}
	}
}

/// Forward PTY output to the SSH channel, then (once output is drained) the exit
/// status, then EOF + close.
async fn forward_output(
	handle: russh::server::Handle,
	channel: ChannelId,
	mut out_rx: mpsc::UnboundedReceiver<Vec<u8>>,
	exit_rx: oneshot::Receiver<u32>,
) {
	while let Some(chunk) = out_rx.recv().await {
		if handle.data(channel, chunk).await.is_err() {
			break;
		}
	}
	if let Ok(code) = exit_rx.await {
		let _ = handle.exit_status_request(channel, code).await;
	}
	let _ = handle.eof(channel).await;
	let _ = handle.close(channel).await;
}
