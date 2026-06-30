<div align="center">

<img src="libretether-desktop/public/libretether.png" alt="LibreTether" width="120" />

# LibreTether

**A self-hosted remote desktop controller that reaches your machines over your own private mesh.**

Enrol a machine with a one-click script, and LibreTether keeps it reachable for as long as
it's on — check its status, run commands, take screenshots, and take over the screen
with full mouse and keyboard control. No cloud service in the middle, no account to
sign up for.

</div>

---

## How it works

LibreTether is two programs that talk over [QUIC](https://en.wikipedia.org/wiki/QUIC):

- **The controller** — this Tauri desktop app. It manages your machines and is the one
  fixed point in the system: it *listens*, and agents dial in to it.
- **The agent** (`libretether-agent`) — a small headless daemon that runs on each machine you
  want to control. It dials the controller and holds the connection open, so the machine
  stays reachable even behind NAT/firewalls (outbound connections "just work").

```
 ┌──────────────┐        agent dials out + key auth        ┌────────────────────┐
 │  Controller  │ ◀─────────────────────────────────────── │  Background agent  │
 │  (this app)  │ ───────────────────────────────────────▶ │  (client machine)  │
 │   listens    │   control · exec · screenshot · input    │  systemd/launchd/… │
 └──────────────┘            live screen frames             └────────────────────┘
```

### Connection modes

Each controller you create has a **type** that sets how it and its clients reach each other
(picked when you create the controller on the launch screen). None require the client to log in:

- **Tailscale** — give the controller a [Tailscale **auth key**](https://tailscale.com/kb/1085/auth-keys).
  Deploy scripts run `tailscale up --authkey=…`, so each client joins
  your tailnet **non-interactively**. NAT traversal is free (Tailscale's DERP relays). The
  controller listens on its tailnet address; agents dial in.
- **Direct** — no Tailscale. Agents dial the controller's advertise address, which must be
  reachable (LAN, an existing VPN, or a port-forward on the controller). Zero third-party
  dependency.
- **Relay (server-backed)** — run **`libretether-relay`** on a public cloud host. The controller
  **and** every client dial *out* to it, so nothing on either side needs to be exposed — the
  relay routes between them. It carries everything: the control plane, the live session, and
  RDP/SSH (tunneled). This is the option for fleets where neither end is reachable.

### Relay setup (`libretether-relay`)

1. On a cloud host with a public IP, build/copy `libretether-relay` (`run relay:build`) and run
   `libretether-relay run`. First run generates a config and prints an **owner secret** and an
   **agent secret**:
   ```
   libretether-relay info       # prints listen address + the two secrets
   ```
2. In the app's launch screen, **New controller → Relay**, give it a name and enter the relay's
   `host:port` and the two secrets, then **Connect**. The controller dials the relay instead of
   listening.
3. Add machines as usual — their deploy scripts enrol against the relay (no Tailscale,
   no exposure). Open UDP 47600 on the cloud host's firewall.

#### Run the relay with Docker

A multi-arch image (`linux/amd64`, `linux/arm64`) is published to GHCR on every release:

```bash
# Generate config + print the secrets (one-time), then run the relay.
docker run --rm -v libretether:/data ghcr.io/libretether/libretether-relay:latest info
docker run -d --name libretether-relay -p 47600:47600/udp \
  -v libretether:/data --restart unless-stopped \
  ghcr.io/libretether/libretether-relay:latest
```

The named volume (`/data`) keeps the generated config — the owner/agent secrets and the
TLS cert — stable across restarts. QUIC is UDP, hence `-p 47600:47600/udp`. Build it
yourself with `run relay:docker:build` (or `docker build -t libretether-relay .`).

To reprint the secrets from an **already-running** container, pass `--config` explicitly —
`docker exec` bypasses the image entrypoint (which supplies it), so the bare
`… info` would look at the wrong path:

```bash
docker exec libretether-relay libretether-relay --config /data/config.json info
```

Authentication is layered: the secrets gate access to the relay, and the agent still proves
its identity to the controller end-to-end with Ed25519 — the relay only forwards bytes.

#### Phone-friendly install (pairing portal)

For someone you're guiding over the phone — who can't paste a command — a relay can also
serve a **browser pairing portal**. In the app, **Add machine → Phone install** mints a short
code; you read out *"go to `your-relay`, code `4F9K-2A7C`"*. They open the site, type the code,
and download a one-click installer — no command, no keys to dictate.

Under the hood this is a [SPAKE2](https://datatracker.ietf.org/doc/html/draft-irtf-cfrg-spake2)
PAKE brokered by the relay (`libretether-portal`, embedded in the relay binary): the code splits
into a nameplate the relay routes on and a password it never sees, so the relay can't read the
enrollment bundle or machine-in-the-middle it, and a wrong code gets a single online guess before
the slot is burned. The controller pins its key exactly as the pasted flow does — no
trust-on-first-use. Both ends show a matching verify phrase as a final cross-check.

The portal must be reached over HTTPS so the page can't be tampered with. The easiest way is to
let the relay get its own Let's Encrypt certificate — **entirely from docker-compose, no config
editing**. Point a DNS record at the host and run:

```yaml
services:
  relay:
    image: ghcr.io/libretether/libretether-relay:latest
    restart: unless-stopped
    volumes: [libretether:/data]
    # The container runs unprivileged, so it listens high inside and we map the public
    # 80/443 to those. 443 must be the public port — ACME (TLS-ALPN-01) validates there.
    ports: ["47600:47600/udp", "80:8080", "443:8443"]
    environment:
      LIBRETETHER_PORTAL_DOMAIN: "relay.example.com"      # the hostname users open
      LIBRETETHER_PORTAL_ACME: "1"                         # get + renew a Let's Encrypt cert
      LIBRETETHER_PORTAL_HTTP_LISTEN: "0.0.0.0:8080"
      LIBRETETHER_PORTAL_HTTPS_LISTEN: "0.0.0.0:8443"
      LIBRETETHER_PORTAL_ACME_CONTACT: "you@example.com"  # optional
      # LIBRETETHER_PORTAL_ACME_STAGING: "1"              # test against LE staging first
volumes: { libretether: {} }
```

The relay obtains and renews the cert itself (caching it in `/data/acme`) and serves the portal on
the public 443, redirecting 80→443. The ACME client is `rustls-acme` pinned to *ring* — no extra
build tooling.

**Other TLS options** (if you'd rather not use the relay's ACME):

- **Your own certificate** — set `tls_cert_path`/`tls_key_path` (PEM) in the `portal` config block
  below and the relay terminates TLS itself.
- **A reverse proxy** — set only `LIBRETETHER_PORTAL_DOMAIN`, leave ACME off, and run the relay
  behind Caddy/Traefik/nginx (which do ACME for you), pointed at the plain-HTTP port (80).

Everything env vars set can also live in the relay config (`/data/config.json`) under a `portal`
block, if you prefer config files — `LIBRETETHER_PORTAL_DOMAIN` overrides `portal.domain` when both
are set:

```jsonc
"portal": {
  "domain": "relay.example.com",
  "tls_cert_path": "/data/fullchain.pem", // optional: relay terminates TLS itself…
  "tls_key_path": "/data/privkey.pem"     // …omit both to serve plain HTTP behind a TLS proxy
}
```

### The background agent

The deploy script installs `libretether-agent` as a **per-user** background service so the
machine is reachable on every boot. Per-user (not a system daemon) matters because screen
capture and input injection must run inside the graphical session:

| OS | Service | Notes |
|----|---------|-------|
| Linux | systemd **user** unit (`libretether-agent.service`) | Needs the graphical session; X11 and Wayland are both supported (see below) |
| macOS | **LaunchAgent** (`com.libretether.agent`) | Requires Screen Recording + Accessibility permissions (granted once in System Settings) |
| Windows | logon **scheduled task** (`LibreTetherAgent`) | Runs in the interactive console session |

### X11 and Wayland

The agent detects the session at runtime and picks a backend:

- **X11** — capture via `xcap`, input via `enigo`. Works out of the box.
- **Wayland** — capture via the **ScreenCast** portal (a PipeWire stream), input via the
  **RemoteDesktop** portal, and one-shot screenshots via the **Screenshot** portal. Both
  live portals share a single session, so the user approves **one** "share your screen"
  prompt per session. This is always built into the Linux agent (it's a Linux-target
  dependency, not a feature flag), so Linux builds need `libpipewire-0.3-dev` (installed by
  `run setup`). macOS/Windows agents have no portals and use the `xcap`/`enigo` path.

### Ways to connect

Every method rides Tailscale straight to the client's private IP — no extra tunneling.

- **Live control (in-app)** — the controller streams frames and injects input over its own
  QUIC session, rendered inside the LibreTether window. On Wayland this uses the portals (one
  consent prompt per connect).
- **RDP** — the **Connect via RDP** button enables an RDP server on the client and launches
  your host's RDP viewer at the client's tailnet IP. On Linux it drives
  **gnome-remote-desktop** (`grdctl`) with generated credentials, so there's **no per-connect
  consent prompt** and it sidesteps the Wayland portal entirely; on Windows it enables the
  built-in Remote Desktop service. Choose your viewer under **Connection → Host tools** — FreeRDP,
  Remmina, GNOME Connections, or a custom command. Requirements: an RDP client on the
  **controller** (FreeRDP installed by `run setup`) and `gnome-remote-desktop` on **Linux
  clients** (installed by the deploy script). macOS has no built-in RDP server.
- **SSH** — the **Connect via SSH** button opens your terminal running `ssh` to the client (as
  the agent's user). **No SSH server needed on the client:** if one is already listening it's
  used as-is, otherwise LibreTether falls back to a **built-in SSH server the agent runs
  in-process** — so SSH works on any machine, including a stock Windows box, with nothing to
  install or enable. The built-in server binds loopback only, is reached through the same
  authenticated tunnel as everything else, and accepts a single ephemeral key the controller
  generates per connect. Pick your terminal under **Connection → Host tools** (or it
  auto-detects gnome-terminal/konsole/xterm/…).

### Security

- Each agent **and** controller has its own **Ed25519** keypair; the private seed never
  leaves the machine.
- Authentication is **mutual** on every connection: the controller issues a random nonce
  and the agent signs it (the controller accepts only a signature matching the public key
  it recorded at enrollment); the agent issues a nonce back and the controller signs it,
  and the agent accepts only the controller key it pinned. The controller's key is **required**
  at enrollment (carried in the deploy command as `--controller-key`) — there is no
  trust-on-first-use, so an agent without a pinned key must be re-enrolled.
- Because both directions are verified at the application layer, the link does not rely on
  the network being trusted — a man-in-the-middle on a Direct-mode port-forward, or a party
  that merely holds the relay's owner secret, can't impersonate the controller and drive an
  agent. After a successful handshake the agent issues a per-connection capability token
  that every control/screen/tunnel stream must carry, so unauthenticated streams (e.g.
  injected through the relay) are rejected. (Such a party still can't drive agents, but the
  owner secret is a single controller-slot credential, so treat it as sensitive for
  *availability* too — whoever holds it can claim the relay's controller slot.)
- A **one-time enrollment token** (baked into the deploy script) binds the very first
  connection, then is burned.
- **Phone pairing** (the browser portal) carries enrollment over a SPAKE2 PAKE keyed by the
  short spoken code. The relay routes the two sides by a nameplate but never learns the
  code's secret half, so it can't read the enrollment bundle or machine-in-the-middle it; the
  controller key is still pinned (no trust-on-first-use), and a wrong code is allowed a single
  online guess before the slot is burned.
- Config files holding secrets (identity seeds, enrollment tokens, relay/owner secrets, the
  TLS key) are written owner-only (`0600`).
- QUIC encrypts the transport (TLS 1.3); on a tailnet the link is end-to-end encrypted on
  top of that. In relay mode the relay forwards bytes between the controller and agents and
  can see the (decrypted) stream contents, so a **trusted relay host** is still assumed.

## Status

This is an early build. What works today:

- ✅ Create / list / remove machines and generate per-OS deploy scripts
- ✅ Agent enrollment, always-on background service, auto-reconnect
- ✅ Live online status, uptime, remote command execution, screenshots
- ✅ **Live screen control** — streamed frames with mouse + keyboard takeover
- ✅ **Wayland support** via XDG portals (X11 still supported too)
- ✅ **RDP connect** — one-click into gnome-remote-desktop / Windows RDP, your choice of viewer
- ✅ **SSH connect** — one-click terminal session; uses the client's `sshd` if present, else a built-in server the agent runs, so no SSH server needs installing
- ✅ **Logs** — in-app Logs page with live controller activity plus on-demand agent logs, filterable by level and searchable
- ✅ **Relay mode** — `libretether-relay` on a cloud host routes between controller and clients (control plane + RDP/SSH tunneled), so neither end is exposed

Releases publish the `libretether-agent` and `libretether-relay` binaries for every platform
(`-linux-x86_64`, `-linux-aarch64`, `-macos-universal`, `-windows-x86_64.exe`). The deploy
script downloads the matching agent from the latest release and verifies it against the
published `.sha256` sidecar. Override the binary source with `LIBRETETHER_AGENT_URL` (a
specific asset) or `LIBRETETHER_AGENT_BIN` (a local build) when developing — a custom URL
with no `.sha256` sidecar skips the integrity check (the installer warns), so only point it
at a source you trust. Grab `libretether-relay-linux-x86_64` for your relay host; the relay
is also published as a multi-arch container image at `ghcr.io/libretether/libretether-relay`
(see [Run the relay with Docker](#run-the-relay-with-docker)).

Rough edges & next up: frame streaming is JPEG-over-QUIC (no delta/codec yet), input
mapping is tuned for the primary display, and the Wayland PipeWire capture
(`libretether-agent/src/pwstream.rs`) benefits from testing across compositors.

## Quick start

This project uses [Runfile](https://github.com/Skiley/runfile) for task running
(`run <target>`). You can also use the underlying `pnpm`/`cargo` commands directly.

```bash
# 1. Install system prerequisites (WebKitGTK, X11/PipeWire capture libs) and JS deps
run setup

# 2. Launch the controller in development (Rust backend + hot-reloading UI)
run dev

# 3. Build a production controller bundle for your OS
run build

# 4. Build the headless agent binary (ship this to the machines you control)
run agent:build     # -> target/release/libretether-agent
```

> Without Runfile: `pnpm install`, then (from `libretether-desktop/`) `pnpm exec tauri dev` /
> `pnpm exec tauri build`, and `cargo build -p libretether-agent --release`.

### Enrolling a machine

1. In the controller, open **Machines → Add machine**, name it and pick its OS.
2. Copy the generated command and run it on the target machine. It's a one-liner that runs
   the published installer (below) with this machine's token and address already filled in —
   the controller just supplies the arguments; the install logic lives only in the release
   installer. To use a local build or a specific asset, prefix it with
   `LIBRETETHER_AGENT_BIN=/path/to/binary` or `LIBRETETHER_AGENT_URL=https://...`.

3. The script joins the machine to Tailscale, installs the agent, enrols it, and starts
   the background service. It shows up as **online** in the controller within seconds.

#### One-line install (without copying a script)

Every release also ships generic, argument-driven installers, so you can enrol a machine
straight from the release with the **token** (and a relay/controller address) the controller
shows you when you add it:

```bash
# Linux / macOS — relay mode
curl -fsSL https://github.com/LibreTether/libretether/releases/latest/download/install-linux.sh \
  | sh -s -- --token <TOKEN> --relay <RELAY_HOST:PORT> --relay-secret <AGENT_SECRET>

# Linux / macOS — direct / Tailscale mode
curl -fsSL .../install-linux.sh | sh -s -- --token <TOKEN> --controller <HOST:PORT> [--tailscale-key <KEY>]
```

```powershell
# Windows (PowerShell)
& ([scriptblock]::Create((irm https://github.com/LibreTether/libretether/releases/latest/download/install-windows.ps1))) `
  -Token <TOKEN> -Relay <RELAY_HOST:PORT> -RelaySecret <AGENT_SECRET>
```

Use `install-macos.sh` on macOS. Each installer is pinned to the release it ships with, so it
always pulls the matching agent build; `LIBRETETHER_AGENT_BIN` / `LIBRETETHER_AGENT_URL` still
override the binary source.

> Trying it on one machine? You can skip the scripts entirely: run
> `libretether-agent enroll --controller <addr> --token <token> --controller-key <key>` then
> `libretether-agent run` (or `libretether-agent install`) by hand. `--controller-key` is
> required (the controller's public key — it's in the generated deploy command).

## Development

```bash
run dev                  # controller with hot reload
run agent:dev run        # run the agent from source (subcommands: enroll/run/status/install)
run check                # biome + tsc + cargo fmt + clippy + license check (the CI gate)
run lint                 # auto-format & fix everything
```

The Runfile is a [workspace](https://github.com/Skiley/runfile): the root `Runfile.json`
owns workspace-wide tasks (`install`, `setup`, `check`, `lint`, `release`, …) and includes one
`Runfile.json` per submodule under a namespace. Each submodule's tasks run in its own directory,
so they're invoked as `<namespace>:<task>` — e.g. `run agent:build`, `run relay:dev run`,
`run relay:docker:build`, `run desktop:build:web`, `run protocol:check`. `run :list` shows them all.

### Layout

A Cargo workspace (root `Cargo.toml`) ties the Rust crates together; the desktop app's
frontend + Tauri shell live in `libretether-desktop/`. Each submodule also carries its own
`Runfile.json` (see [Development](#development)).

```
libretether-desktop/     desktop controller app
  src/                   React UI (Vite + Tailwind v4)
  src-tauri/             controller backend — QUIC server, registry, deploy scripts, commands
libretether-protocol/    shared wire protocol, QUIC transport, Ed25519 identity
libretether-agent/       headless libretether-agent daemon (capture, input, service install)
libretether-relay/       libretether-relay relay (optional, for relay mode)
```

## License

GNU Affero General Public License v3.0 — see [LICENSE](LICENSE).

Copyright (C) 2026 LibreTether contributors. This program is free software:
you can redistribute it and/or modify it under the terms of the AGPL as
published by the Free Software Foundation, either version 3 of the License.
Because LibreTether runs as a network service, the AGPL's section 13 applies:
if you offer a modified version over a network, you must also offer its source.
