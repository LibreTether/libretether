//! The relay's HTTP server: it serves the embedded `libretether-portal` SPA so a
//! not-yet-enrolled machine can just open `https://<relay>/` in a browser, type the
//! spoken pairing code, and download a ready-to-run installer (see
//! [`libretether_protocol::pairing`]). This is the *only* HTTP the relay speaks; the
//! actual pairing runs over QUIC.
//!
//! TLS comes three ways, in priority order: (1) `LIBRETETHER_PORTAL_ACME` env var →
//! an auto-obtained & renewed Let's Encrypt cert (TLS-ALPN-01); (2) an operator
//! cert in the config → the relay terminates TLS itself; (3) neither → plain HTTP,
//! for behind a TLS-terminating reverse proxy (Caddy/Traefik/nginx). Either way the
//! SPA's integrity is protected by TLS in front of the browser. The ACME client is
//! `rustls-acme` pinned to **ring** (no aws-lc / cmake), matching the rest of the
//! workspace's crypto.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use include_dir::{include_dir, Dir};
use rustls_acme::caches::DirCache;
use rustls_acme::AcmeConfig;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig as TlsServerConfig;
use tokio_rustls::TlsAcceptor;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_stream::StreamExt;

use crate::logbook;

/// Env var setting the portal's public domain. Lets the whole portal be configured
/// from docker-compose without hand-editing the relay's config file.
const PORTAL_DOMAIN_ENV: &str = "LIBRETETHER_PORTAL_DOMAIN";
/// Env overrides for the listen addresses — handy in Docker, where the container
/// runs unprivileged and can't bind 80/443, so you listen high (e.g. `0.0.0.0:8443`)
/// and map `443→8443` in compose.
const PORTAL_HTTP_LISTEN_ENV: &str = "LIBRETETHER_PORTAL_HTTP_LISTEN";
const PORTAL_HTTPS_LISTEN_ENV: &str = "LIBRETETHER_PORTAL_HTTPS_LISTEN";
/// Env var that turns on automatic Let's Encrypt certificates (set in the relay's
/// docker-compose). Opt-in: without it the portal uses a provided cert or plain HTTP.
const ACME_ENV: &str = "LIBRETETHER_PORTAL_ACME";
/// Optional ACME account contact (e.g. `admin@example.com`); a `mailto:` is added if absent.
const ACME_CONTACT_ENV: &str = "LIBRETETHER_PORTAL_ACME_CONTACT";
/// Set to use Let's Encrypt's *staging* directory (for testing — avoids rate limits).
const ACME_STAGING_ENV: &str = "LIBRETETHER_PORTAL_ACME_STAGING";
/// Override where issued certs/account keys are cached (default: `<data dir>/acme`).
const ACME_CACHE_ENV: &str = "LIBRETETHER_PORTAL_ACME_CACHE";

/// The portal SPA, embedded into the relay binary so the relay is a single
/// deployable. It's a hand-authored static page (no build step) under
/// `libretether-portal/static`.
static PORTAL: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../libretether-portal/static");

/// Hard cap on the request head we'll buffer before giving up — this is a tiny
/// static-GET surface, so anything larger is junk we refuse rather than allocate for.
const MAX_REQUEST_HEAD: usize = 16 * 1024;

/// How the HTTP listener responds: serve the SPA, or 301-redirect to the HTTPS site.
#[derive(Clone)]
pub enum HttpMode {
	/// Serve the embedded SPA (used on the HTTPS port, or on plain HTTP when behind a
	/// TLS-terminating proxy).
	Serve,
	/// Redirect every request to `https://{domain}{path}` (used on port 80 when the
	/// relay terminates TLS itself).
	RedirectToHttps { domain: String },
}

/// Portal HTTP/HTTPS settings (the `portal` block of the relay config).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct PortalConfig {
	/// Public hostname users open and the target of the 80→443 redirect.
	pub domain: String,
	/// Plain-HTTP listen address. Behind a TLS-terminating reverse proxy, point the
	/// proxy here and leave the `tls_*` fields unset.
	#[serde(default = "default_http_listen")]
	pub http_listen: String,
	/// PEM certificate chain for direct TLS. When this and `tls_key_path` are both
	/// set, the relay serves HTTPS itself and the HTTP port only redirects.
	#[serde(default)]
	pub tls_cert_path: Option<String>,
	/// PEM private key for direct TLS.
	#[serde(default)]
	pub tls_key_path: Option<String>,
	/// HTTPS listen address (used only with a cert).
	#[serde(default = "default_https_listen")]
	pub https_listen: String,
}

// Dual-stack by default (`[::]` takes IPv6 and, on a normal host, IPv4-mapped too),
// so an IPv6 (or IPv6-only) client — including Let's Encrypt validating over a AAAA
// record — is served without extra config. Override via the `_LISTEN` env vars.
fn default_http_listen() -> String {
	"[::]:80".to_string()
}

fn default_https_listen() -> String {
	"[::]:443".to_string()
}

impl Default for PortalConfig {
	fn default() -> Self {
		Self {
			domain: String::new(),
			http_listen: default_http_listen(),
			tls_cert_path: None,
			tls_key_path: None,
			https_listen: default_https_listen(),
		}
	}
}

/// Portal settings pulled from the environment, layered over the config file by
/// [`resolve_config`] — so a relay can be brought up entirely from docker-compose.
struct PortalEnv {
	domain: Option<String>,
	http_listen: Option<String>,
	https_listen: Option<String>,
	acme: bool,
}

impl PortalEnv {
	fn from_env() -> Self {
		let nonblank = |name| std::env::var(name).ok().filter(|s: &String| !s.trim().is_empty());
		Self {
			domain: nonblank(PORTAL_DOMAIN_ENV),
			http_listen: nonblank(PORTAL_HTTP_LISTEN_ENV),
			https_listen: nonblank(PORTAL_HTTPS_LISTEN_ENV),
			acme: truthy(std::env::var(ACME_ENV).ok().as_deref()),
		}
	}
}

/// Resolve the effective portal config from the config-file value and the
/// environment. The portal is on when the config file has a `portal` block **or**
/// when `LIBRETETHER_PORTAL_DOMAIN`/`LIBRETETHER_PORTAL_ACME` is set — so a fresh
/// relay can be brought up entirely from docker-compose env vars, no file editing.
/// Env values override the file's. Returns `None` when the portal is off.
pub fn resolve_config(file: Option<PortalConfig>) -> Option<PortalConfig> {
	resolve_config_inner(file, PortalEnv::from_env())
}

fn resolve_config_inner(file: Option<PortalConfig>, env: PortalEnv) -> Option<PortalConfig> {
	if file.is_none() && env.domain.is_none() && !env.acme {
		return None;
	}
	let mut cfg = file.unwrap_or_default();
	if let Some(domain) = env.domain {
		cfg.domain = domain;
	}
	if let Some(http_listen) = env.http_listen {
		cfg.http_listen = http_listen;
	}
	if let Some(https_listen) = env.https_listen {
		cfg.https_listen = https_listen;
	}
	Some(cfg)
}

/// Bind the portal listeners and serve forever. Three TLS modes, in priority order:
/// (1) the `LIBRETETHER_PORTAL_ACME` env var → an auto-obtained Let's Encrypt cert;
/// (2) an operator-provided cert in the config → the relay terminates TLS itself;
/// (3) neither → plain HTTP, for behind a TLS-terminating reverse proxy. `data_dir`
/// is where ACME caches the issued cert (so it survives restarts). Returns only on a
/// bind error.
pub async fn run(cfg: PortalConfig, data_dir: PathBuf) -> Result<()> {
	if truthy(std::env::var(ACME_ENV).ok().as_deref()) {
		return run_acme(cfg, data_dir).await;
	}
	match tls_acceptor(&cfg)? {
		Some(acceptor) => {
			let https = TcpListener::bind(&cfg.https_listen)
				.await
				.with_context(|| format!("binding portal HTTPS on {}", cfg.https_listen))?;
			let http = TcpListener::bind(&cfg.http_listen)
				.await
				.with_context(|| format!("binding portal HTTP on {}", cfg.http_listen))?;
			logbook::info(&format!(
				"portal serving HTTPS on {} (HTTP on {} redirects to it)",
				cfg.https_listen, cfg.http_listen
			));
			tokio::join!(
				accept_loop(https, Some(acceptor), HttpMode::Serve),
				accept_loop(
					http,
					None,
					HttpMode::RedirectToHttps {
						domain: cfg.domain.clone()
					}
				),
			);
		}
		None => {
			let http = TcpListener::bind(&cfg.http_listen)
				.await
				.with_context(|| format!("binding portal HTTP on {}", cfg.http_listen))?;
			logbook::info(&format!(
				"portal serving HTTP on {} (put TLS in front, e.g. a reverse proxy)",
				cfg.http_listen
			));
			accept_loop(http, None, HttpMode::Serve).await;
		}
	}
	Ok(())
}

/// ACME settings resolved from the environment (everything except the on/off toggle).
struct AcmeSettings {
	cache_dir: PathBuf,
	production: bool,
	contacts: Vec<String>,
}

impl AcmeSettings {
	fn from_env(data_dir: &Path) -> Self {
		Self {
			cache_dir: acme_cache_dir(std::env::var(ACME_CACHE_ENV).ok(), data_dir),
			// Default to production Let's Encrypt; opt into staging for testing.
			production: !truthy(std::env::var(ACME_STAGING_ENV).ok().as_deref()),
			contacts: parse_contacts(std::env::var(ACME_CONTACT_ENV).ok().as_deref()),
		}
	}
}

/// Serve HTTPS with an automatically obtained & renewed Let's Encrypt certificate
/// (TLS-ALPN-01 on the HTTPS port), with the HTTP port redirecting to it. The cert
/// and account key are cached under the data dir so they persist across restarts.
///
/// Certificate acquisition/renewal is folded into the accept stream by `rustls-acme`
/// (no background task). This path needs a real public domain + Let's Encrypt, so it
/// isn't exercised in tests; the env/path parsing it depends on lives in the pure
/// helpers below, which are.
async fn run_acme(cfg: PortalConfig, data_dir: PathBuf) -> Result<()> {
	libretether_protocol::tls::install_crypto_provider();
	if cfg.domain.trim().is_empty() {
		anyhow::bail!(
			"{ACME_ENV} is set but no domain is configured — set {PORTAL_DOMAIN_ENV} (or portal.domain in the config) to the hostname to request a certificate for"
		);
	}
	let settings = AcmeSettings::from_env(&data_dir);
	std::fs::create_dir_all(&settings.cache_dir)
		.with_context(|| format!("creating the ACME cache dir {}", settings.cache_dir.display()))?;

	let https = TcpListener::bind(&cfg.https_listen)
		.await
		.with_context(|| format!("binding portal HTTPS on {}", cfg.https_listen))?;
	let http = TcpListener::bind(&cfg.http_listen)
		.await
		.with_context(|| format!("binding portal HTTP on {}", cfg.http_listen))?;
	logbook::info(&format!(
		"portal serving HTTPS on {} via ACME ({}) for {}; HTTP on {} redirects to it",
		cfg.https_listen,
		if settings.production {
			"Let's Encrypt"
		} else {
			"Let's Encrypt staging"
		},
		cfg.domain,
		cfg.http_listen,
	));

	let redirect = accept_loop(
		http,
		None,
		HttpMode::RedirectToHttps {
			domain: cfg.domain.clone(),
		},
	);
	let mut tls_incoming = AcmeConfig::new([cfg.domain.clone()])
		.contact(settings.contacts.iter())
		.cache(DirCache::new(settings.cache_dir))
		.directory_lets_encrypt(settings.production)
		.tokio_incoming(TcpListenerStream::new(https), vec![b"http/1.1".to_vec()]);
	let serve = async {
		while let Some(tls) = tls_incoming.next().await {
			match tls {
				// Each connection is a ready TLS stream (the ACME challenges the library
				// handles itself never surface here).
				Ok(tls) => {
					tokio::spawn(async move {
						let _ = handle_http(tls, HttpMode::Serve).await;
					});
				}
				Err(e) => logbook::warn(&format!("portal TLS/ACME error: {e:?}")),
			}
		}
	};
	tokio::join!(redirect, serve);
	Ok(())
}

/// Whether an env-var value reads as "on" (`1`/`true`/`yes`/`on`, case-insensitive).
fn truthy(value: Option<&str>) -> bool {
	matches!(
		value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
		Some("1" | "true" | "yes" | "on")
	)
}

/// Prefix a bare contact with `mailto:` (Let's Encrypt wants a URI), leaving an
/// already-schemed value alone.
fn ensure_mailto(contact: &str) -> String {
	if contact.contains(':') {
		contact.to_string()
	} else {
		format!("mailto:{contact}")
	}
}

/// Parse the comma-separated `LIBRETETHER_PORTAL_ACME_CONTACT` into `mailto:` URIs,
/// dropping blanks.
fn parse_contacts(raw: Option<&str>) -> Vec<String> {
	raw.unwrap_or_default()
		.split(',')
		.map(str::trim)
		.filter(|s| !s.is_empty())
		.map(ensure_mailto)
		.collect()
}

/// Where ACME caches the issued cert + account key: the override if set, else
/// `<data dir>/acme` (so it lives in the relay's persisted volume next to the config).
fn acme_cache_dir(override_path: Option<String>, data_dir: &Path) -> PathBuf {
	override_path
		.filter(|s| !s.trim().is_empty())
		.map(PathBuf::from)
		.unwrap_or_else(|| data_dir.join("acme"))
}

/// Build a TLS acceptor from the operator's PEM cert+key, or `None` when no cert is
/// configured (plain-HTTP / behind-proxy mode).
fn tls_acceptor(cfg: &PortalConfig) -> Result<Option<TlsAcceptor>> {
	match (&cfg.tls_cert_path, &cfg.tls_key_path) {
		(Some(cert), Some(key)) => Ok(Some(TlsAcceptor::from(tls_config(Path::new(cert), Path::new(key))?))),
		(None, None) => Ok(None),
		_ => anyhow::bail!("portal tls_cert_path and tls_key_path must be set together"),
	}
}

/// Load a PEM cert chain + private key into a rustls server config (HTTP/1.1 ALPN).
fn tls_config(cert_path: &Path, key_path: &Path) -> Result<Arc<TlsServerConfig>> {
	libretether_protocol::tls::install_crypto_provider();
	let cert_pem = std::fs::read(cert_path).with_context(|| format!("reading {}", cert_path.display()))?;
	let key_pem = std::fs::read(key_path).with_context(|| format!("reading {}", key_path.display()))?;
	let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_pem.as_slice())
		.collect::<std::result::Result<_, _>>()
		.context("parsing the portal TLS certificate")?;
	if certs.is_empty() {
		anyhow::bail!("no certificates found in {}", cert_path.display());
	}
	let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_pem.as_slice())
		.context("parsing the portal TLS private key")?
		.with_context(|| format!("no private key found in {}", key_path.display()))?;
	let mut config = TlsServerConfig::builder()
		.with_no_client_auth()
		.with_single_cert(certs, key)
		.context("building the portal TLS config")?;
	config.alpn_protocols = vec![b"http/1.1".to_vec()];
	Ok(Arc::new(config))
}

/// Accept connections forever, handing each to [`handle_http`] (wrapped in TLS when
/// an acceptor is given). One request per connection — we always respond
/// `Connection: close`.
async fn accept_loop(listener: TcpListener, acceptor: Option<TlsAcceptor>, mode: HttpMode) {
	loop {
		let Ok((tcp, _peer)) = listener.accept().await else {
			continue;
		};
		let (acceptor, mode) = (acceptor.clone(), mode.clone());
		tokio::spawn(async move {
			match acceptor {
				Some(acceptor) => match acceptor.accept(tcp).await {
					Ok(tls) => {
						let _ = handle_http(tls, mode).await;
					}
					Err(_) => { /* TLS handshake failed (probe, bad client) — drop it */ }
				},
				None => {
					let _ = handle_http(tcp, mode).await;
				}
			}
		});
	}
}

/// Read one HTTP request head, route it, and write the response. Only `GET`/`HEAD`
/// are served; the body (if any) is ignored. Generic over the stream so it's the
/// same code for plain TCP and TLS, and unit-testable over an in-memory duplex.
pub async fn handle_http<S>(mut stream: S, mode: HttpMode) -> std::io::Result<()>
where
	S: AsyncRead + AsyncWrite + Unpin,
{
	// Read until the end of the request head (CRLFCRLF), bounded by MAX_REQUEST_HEAD.
	let mut buf = Vec::with_capacity(1024);
	let mut chunk = [0u8; 1024];
	loop {
		if buf.windows(4).any(|w| w == b"\r\n\r\n") {
			break;
		}
		if buf.len() > MAX_REQUEST_HEAD {
			return write_response(
				&mut stream,
				413,
				"Payload Too Large",
				"text/plain",
				b"request too large",
				true,
			)
			.await;
		}
		let n = stream.read(&mut chunk).await?;
		if n == 0 {
			return Ok(()); // client hung up before sending a full request
		}
		buf.extend_from_slice(&chunk[..n]);
	}

	let mut headers = [httparse::EMPTY_HEADER; 32];
	let mut req = httparse::Request::new(&mut headers);
	if req.parse(&buf).ok().map(|s| s.is_complete()) != Some(true) {
		return write_response(&mut stream, 400, "Bad Request", "text/plain", b"bad request", true).await;
	}
	let method = req.method.unwrap_or("");
	let raw_path = req.path.unwrap_or("/");
	// Drop any query string; routing is by path only.
	let path = raw_path.split(['?', '#']).next().unwrap_or("/");

	if !matches!(method, "GET" | "HEAD") {
		return write_response(
			&mut stream,
			405,
			"Method Not Allowed",
			"text/plain",
			b"method not allowed",
			true,
		)
		.await;
	}
	let head_only = method == "HEAD";

	match &mode {
		HttpMode::RedirectToHttps { domain } => {
			let location = format!("https://{domain}{}", if raw_path.is_empty() { "/" } else { raw_path });
			write_redirect(&mut stream, &location, head_only).await
		}
		HttpMode::Serve => {
			let (content_type, body) = resolve(path);
			write_response(&mut stream, 200, "OK", content_type, body, head_only).await
		}
	}
}

/// Resolve a request path to an embedded asset, falling back to the SPA's
/// `index.html` so client-side routes (and a bare `/`) load the app. Path traversal
/// (`..`) is rejected — it just falls back to the index.
fn resolve(path: &str) -> (&'static str, &'static [u8]) {
	let trimmed = path.trim_start_matches('/');
	let index = || {
		PORTAL
			.get_file("index.html")
			.map(|f| ("text/html; charset=utf-8", f.contents()))
			.unwrap_or((
				"text/html; charset=utf-8",
				b"<!doctype html><title>LibreTether</title>" as &[u8],
			))
	};
	if trimmed.is_empty() || trimmed.split('/').any(|seg| seg == "..") {
		return index();
	}
	match PORTAL.get_file(trimmed) {
		Some(file) => (content_type(trimmed), file.contents()),
		None => index(),
	}
}

/// Guess a content type from a file extension. Only the handful the SPA ships.
fn content_type(path: &str) -> &'static str {
	match path.rsplit('.').next() {
		Some("html") => "text/html; charset=utf-8",
		Some("js" | "mjs") => "text/javascript; charset=utf-8",
		Some("css") => "text/css; charset=utf-8",
		Some("svg") => "image/svg+xml",
		Some("json") => "application/json",
		Some("ico") => "image/x-icon",
		Some("png") => "image/png",
		Some("webp") => "image/webp",
		Some("woff2") => "font/woff2",
		Some("txt") => "text/plain; charset=utf-8",
		_ => "application/octet-stream",
	}
}

async fn write_response<S>(
	stream: &mut S,
	status: u16,
	reason: &str,
	content_type: &str,
	body: &[u8],
	head_only: bool,
) -> std::io::Result<()>
where
	S: AsyncWrite + Unpin,
{
	let head = format!(
		"HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
		 Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
		body.len()
	);
	stream.write_all(head.as_bytes()).await?;
	if !head_only {
		stream.write_all(body).await?;
	}
	stream.flush().await?;
	let _ = stream.shutdown().await;
	Ok(())
}

async fn write_redirect<S>(stream: &mut S, location: &str, head_only: bool) -> std::io::Result<()>
where
	S: AsyncWrite + Unpin,
{
	let head = format!(
		"HTTP/1.1 301 Moved Permanently\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
	);
	stream.write_all(head.as_bytes()).await?;
	let _ = head_only;
	stream.flush().await?;
	let _ = stream.shutdown().await;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

	/// Drive `handle_http` over an in-memory duplex with `request` and return the raw
	/// response bytes.
	async fn exchange(request: &str, mode: HttpMode) -> String {
		let (mut client, server) = duplex(64 * 1024);
		let handler = tokio::spawn(handle_http(server, mode));
		client.write_all(request.as_bytes()).await.unwrap();
		let mut out = Vec::new();
		client.read_to_end(&mut out).await.unwrap();
		handler.await.unwrap().unwrap();
		String::from_utf8_lossy(&out).into_owned()
	}

	#[tokio::test]
	async fn serves_index_for_root() {
		let resp = exchange("GET / HTTP/1.1\r\nHost: x\r\n\r\n", HttpMode::Serve).await;
		assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
		assert!(resp.contains("Content-Type: text/html"), "{resp}");
		assert!(resp.contains("LibreTether"), "the index body is served: {resp}");
	}

	#[tokio::test]
	async fn unknown_path_falls_back_to_the_spa_index() {
		// Client-side routes must load the app, not 404.
		let resp = exchange("GET /pair/4F9K HTTP/1.1\r\nHost: x\r\n\r\n", HttpMode::Serve).await;
		assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
		assert!(resp.contains("text/html"), "{resp}");
	}

	#[tokio::test]
	async fn strips_the_query_string_when_routing() {
		let resp = exchange("GET /?code=4F9K-2A7C HTTP/1.1\r\nHost: x\r\n\r\n", HttpMode::Serve).await;
		assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
	}

	#[tokio::test]
	async fn head_returns_headers_without_a_body() {
		let resp = exchange("HEAD / HTTP/1.1\r\nHost: x\r\n\r\n", HttpMode::Serve).await;
		let (head, body) = resp.split_once("\r\n\r\n").unwrap();
		assert!(head.starts_with("HTTP/1.1 200 OK"));
		assert!(head.contains("Content-Length:"));
		assert!(body.is_empty(), "HEAD must not send a body, got {body:?}");
	}

	#[tokio::test]
	async fn rejects_non_get_methods() {
		let resp = exchange(
			"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
			HttpMode::Serve,
		)
		.await;
		assert!(resp.starts_with("HTTP/1.1 405"), "{resp}");
	}

	#[tokio::test]
	async fn redirect_mode_301s_to_https_preserving_the_path() {
		let mode = HttpMode::RedirectToHttps {
			domain: "relay.example.com".into(),
		};
		let resp = exchange("GET /pair?code=4F9K-2A7C HTTP/1.1\r\nHost: x\r\n\r\n", mode).await;
		assert!(resp.starts_with("HTTP/1.1 301"), "{resp}");
		assert!(
			resp.contains("Location: https://relay.example.com/pair?code=4F9K-2A7C"),
			"redirect preserves host (from config) and the original path+query: {resp}"
		);
	}

	#[test]
	fn content_type_maps_known_extensions() {
		assert_eq!(content_type("app.js"), "text/javascript; charset=utf-8");
		assert_eq!(content_type("style.css"), "text/css; charset=utf-8");
		assert_eq!(content_type("logo.svg"), "image/svg+xml");
		assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
		assert_eq!(content_type("whatever.bin"), "application/octet-stream");
	}

	#[test]
	fn resolve_rejects_path_traversal_by_falling_back_to_index() {
		let (ct, _body) = resolve("/../../etc/passwd");
		assert_eq!(ct, "text/html; charset=utf-8", "traversal falls back to the SPA index");
	}

	// ---------------------------------------------------------------- ACME config

	#[test]
	fn truthy_only_accepts_on_values() {
		for v in ["1", "true", "TRUE", "yes", "On", " on "] {
			assert!(truthy(Some(v)), "{v:?} should enable");
		}
		for v in [None, Some(""), Some("0"), Some("false"), Some("off"), Some("maybe")] {
			assert!(!truthy(v), "{v:?} should not enable");
		}
	}

	#[test]
	fn contacts_parse_into_mailto_uris() {
		// Bare emails get a mailto:, already-schemed ones are left alone, blanks dropped.
		assert_eq!(
			parse_contacts(Some("admin@example.com, mailto:ops@example.com ,  ")),
			vec!["mailto:admin@example.com", "mailto:ops@example.com"]
		);
		assert!(parse_contacts(None).is_empty());
		assert!(parse_contacts(Some("   ")).is_empty());
	}

	#[test]
	fn acme_cache_dir_defaults_under_the_data_dir() {
		let data = Path::new("/data");
		assert_eq!(acme_cache_dir(None, data), Path::new("/data/acme"));
		assert_eq!(acme_cache_dir(Some("  ".into()), data), Path::new("/data/acme"));
		// An explicit override wins.
		assert_eq!(acme_cache_dir(Some("/certs".into()), data), Path::new("/certs"));
	}

	#[test]
	fn resolve_config_turns_the_portal_on_from_env_or_file() {
		let env = |domain: Option<&str>, acme: bool| PortalEnv {
			domain: domain.map(Into::into),
			http_listen: None,
			https_listen: None,
			acme,
		};
		// Off: nothing configured anywhere.
		assert!(resolve_config_inner(None, env(None, false)).is_none());
		// Env domain alone enables it (fresh relay, no config edit needed).
		assert_eq!(
			resolve_config_inner(None, env(Some("relay.example.com"), false)).map(|c| c.domain),
			Some("relay.example.com".into())
		);
		// ACME on with no domain still enables the portal (run_acme then reports the
		// missing domain with an actionable error).
		let acme_only = resolve_config_inner(None, env(None, true)).expect("acme enables the portal");
		assert!(acme_only.domain.is_empty());
		// A file block with no env override is used as-is.
		assert_eq!(
			resolve_config_inner(Some(PortalConfig::default()), env(None, false)).map(|c| c.domain),
			Some(String::new())
		);
	}

	#[test]
	fn resolve_config_lets_env_override_domain_and_listen_addresses() {
		// The Docker case: listen high inside the container (so the unprivileged user
		// can bind) and let env override everything, no config file needed.
		let merged = resolve_config_inner(
			Some(PortalConfig {
				domain: "old.example.com".into(),
				..PortalConfig::default()
			}),
			PortalEnv {
				domain: Some("relay.example.com".into()),
				http_listen: Some("0.0.0.0:8080".into()),
				https_listen: Some("0.0.0.0:8443".into()),
				acme: true,
			},
		)
		.unwrap();
		assert_eq!(merged.domain, "relay.example.com");
		assert_eq!(merged.http_listen, "0.0.0.0:8080");
		assert_eq!(merged.https_listen, "0.0.0.0:8443");
	}

	// ---------------------------------------------------------------- TLS path

	/// Generate a throwaway self-signed cert+key as PEM (for the TLS tests).
	fn self_signed_pem() -> (String, String) {
		let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
		(cert.cert.pem(), cert.key_pair.serialize_pem())
	}

	#[test]
	fn tls_config_loads_a_pem_cert_and_key() {
		let dir = std::env::temp_dir().join(format!("lt-portal-tls-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let (cert_pem, key_pem) = self_signed_pem();
		let cert_path = dir.join("cert.pem");
		let key_path = dir.join("key.pem");
		std::fs::write(&cert_path, &cert_pem).unwrap();
		std::fs::write(&key_path, &key_pem).unwrap();
		assert!(tls_config(&cert_path, &key_path).is_ok(), "a valid PEM cert+key loads");
		// Garbage where a key should be is rejected, not silently accepted.
		std::fs::write(&key_path, b"not a key").unwrap();
		assert!(tls_config(&cert_path, &key_path).is_err());
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[tokio::test]
	async fn serves_the_spa_over_real_tls() {
		use tokio_rustls::rustls::{self, pki_types::ServerName};
		use tokio_rustls::TlsConnector;

		libretether_protocol::tls::install_crypto_provider();
		let (cert_pem, key_pem) = self_signed_pem();
		let dir = std::env::temp_dir().join(format!("lt-portal-tlsrt-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let cert_path = dir.join("c.pem");
		let key_path = dir.join("k.pem");
		std::fs::write(&cert_path, &cert_pem).unwrap();
		std::fs::write(&key_path, &key_pem).unwrap();
		let acceptor = TlsAcceptor::from(tls_config(&cert_path, &key_path).unwrap());

		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		tokio::spawn(async move {
			let (tcp, _) = listener.accept().await.unwrap();
			let tls = acceptor.accept(tcp).await.unwrap();
			handle_http(tls, HttpMode::Serve).await.unwrap();
		});

		// A browser-like client: trust nothing in particular (test-only NoVerify), so
		// the test exercises real TLS without minting a CA.
		let client_cfg = rustls::ClientConfig::builder()
			.dangerous()
			.with_custom_certificate_verifier(Arc::new(NoVerify))
			.with_no_client_auth();
		let connector = TlsConnector::from(Arc::new(client_cfg));
		let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
		let mut tls = connector
			.connect(ServerName::try_from("localhost").unwrap(), tcp)
			.await
			.unwrap();
		tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
			.await
			.unwrap();
		let mut out = Vec::new();
		tls.read_to_end(&mut out).await.unwrap();
		let resp = String::from_utf8_lossy(&out);
		assert!(resp.starts_with("HTTP/1.1 200 OK"), "served the SPA over TLS: {resp}");
		assert!(resp.contains("LibreTether"));
		let _ = std::fs::remove_dir_all(&dir);
	}

	/// Test-only client verifier that accepts any server cert (the test uses a
	/// throwaway self-signed cert; mirrors the QUIC layer's `NoVerify`).
	#[derive(Debug)]
	struct NoVerify;

	impl tokio_rustls::rustls::client::danger::ServerCertVerifier for NoVerify {
		fn verify_server_cert(
			&self,
			_e: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
			_i: &[tokio_rustls::rustls::pki_types::CertificateDer<'_>],
			_s: &tokio_rustls::rustls::pki_types::ServerName<'_>,
			_o: &[u8],
			_n: tokio_rustls::rustls::pki_types::UnixTime,
		) -> std::result::Result<tokio_rustls::rustls::client::danger::ServerCertVerified, tokio_rustls::rustls::Error>
		{
			Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
		}
		fn verify_tls12_signature(
			&self,
			_m: &[u8],
			_c: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
			_d: &tokio_rustls::rustls::DigitallySignedStruct,
		) -> std::result::Result<
			tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
			tokio_rustls::rustls::Error,
		> {
			Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
		}
		fn verify_tls13_signature(
			&self,
			_m: &[u8],
			_c: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
			_d: &tokio_rustls::rustls::DigitallySignedStruct,
		) -> std::result::Result<
			tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
			tokio_rustls::rustls::Error,
		> {
			Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
		}
		fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
			vec![
				tokio_rustls::rustls::SignatureScheme::ED25519,
				tokio_rustls::rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
				tokio_rustls::rustls::SignatureScheme::RSA_PKCS1_SHA256,
				tokio_rustls::rustls::SignatureScheme::RSA_PSS_SHA256,
			]
		}
	}
}
