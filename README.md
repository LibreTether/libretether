<div align="center">

<img src="libretether-desktop/public/libretether.svg" alt="LibreTether" width="120" />

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

On the **Controller** page you pick how the controller and clients reach each other. None
of the modes require the client to log in:

- **Tailscale** — paste a [Tailscale **auth key**](https://tailscale.com/kb/1085/auth-keys)
  into the controller. Deploy scripts run `tailscale up --authkey=…`, so each client joins
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

1. On a cloud host with a public IP, build/copy `libretether-relay` (`run build:relay`) and run
   `libretether-relay run`. First run generates a config and prints an **owner secret** and an
   **agent secret**:
   ```
   libretether-relay info       # prints listen address + the two secrets
   ```
2. On the **Controller** page → **Relay**, enter the relay's `host:port` and the two secrets,
   save, and restart LibreTether. The controller now dials the relay instead of listening.
3. Add machines as usual — their deploy scripts now enrol against the relay (no Tailscale,
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
yourself with `run docker:build` (or `docker build -t libretether-relay .`).

Authentication is layered: the secrets gate access to the relay, and the agent still proves
its identity to the controller end-to-end with Ed25519 — the relay only forwards bytes.

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
  built-in Remote Desktop service. Choose your viewer on the **Controller** page — FreeRDP,
  Remmina, GNOME Connections, or a custom command. Requirements: an RDP client on the
  **controller** (FreeRDP installed by `run setup`) and `gnome-remote-desktop` on **Linux
  clients** (installed by the deploy script). macOS has no built-in RDP server.
- **SSH** — the **Connect via SSH** button opens your terminal running `ssh` to the client's
  tailnet IP (as the agent's user). The client needs `sshd`; pick your terminal on the
  Controller page (or it auto-detects gnome-terminal/konsole/xterm/…).

### Security

- Each agent has its own **Ed25519** keypair; the private seed never leaves the machine.
- On every connection the controller issues a random nonce and the agent signs it; the
  controller only accepts a signature matching the public key it recorded at enrollment.
- A **one-time enrollment token** (baked into the deploy script) binds the very first
  connection, then is burned.
- QUIC encrypts the transport (TLS 1.3); on a tailnet the link is end-to-end encrypted on
  top of that.

## Status

This is an early build. What works today:

- ✅ Create / list / remove machines and generate per-OS deploy scripts
- ✅ Agent enrollment, always-on background service, auto-reconnect
- ✅ Live online status, uptime, remote command execution, screenshots
- ✅ **Live screen control** — streamed frames with mouse + keyboard takeover
- ✅ **Wayland support** via XDG portals (X11 still supported too)
- ✅ **RDP connect** — one-click into gnome-remote-desktop / Windows RDP, your choice of viewer
- ✅ **SSH connect** — one-click terminal session to the client over the tailnet
- ✅ **Relay mode** — `libretether-relay` on a cloud host routes between controller and clients (control plane + RDP/SSH tunneled), so neither end is exposed

Releases publish the `libretether-agent` and `libretether-relay` binaries for every platform
(`-linux-x86_64`, `-linux-aarch64`, `-macos-universal`, `-windows-x86_64.exe`) — point the
deploy script's `LIBRETETHER_AGENT_URL` at the agent asset (or use a local build via
`LIBRETETHER_AGENT_BIN`), and grab `libretether-relay-linux-x86_64` for your relay host. The relay is
also published as a multi-arch container image at
`ghcr.io/libretether/libretether-relay` (see [Run the relay with Docker](#run-the-relay-with-docker)).

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
run build:agent     # -> target/release/libretether-agent
```

> Without Runfile: `pnpm install`, then (from `libretether-desktop/`) `pnpm exec tauri dev` /
> `pnpm exec tauri build`, and `cargo build -p libretether-agent --release`.

### Enrolling a machine

1. In the controller, open **Machines → Add machine**, name it and pick its OS.
2. Copy or save the generated deploy script and run it on the target machine. Point it at
   the agent binary first:

   ```bash
   # On the client machine, with the built agent next to you:
   LIBRETETHER_AGENT_BIN=/path/to/libretether-agent bash libretether-deploy-<name>.sh
   ```

3. The script joins the machine to Tailscale, installs the agent, enrols it, and starts
   the background service. It shows up as **online** in the controller within seconds.

> Trying it on one machine? You can skip the script: run
> `libretether-agent enroll --controller <addr> --token <token>` then `libretether-agent run`
> (or `libretether-agent install`) by hand.

## Development

```bash
run dev           # controller with hot reload
run dev:agent -- run     # run the agent from source (subcommands: enroll/run/status/install)
run check         # biome + tsc + cargo fmt + clippy (the CI gate)
run lint          # auto-format & fix everything
```

### Layout

A Cargo workspace (root `Cargo.toml`) ties the Rust crates together; the desktop app's
frontend + Tauri shell live in `libretether-desktop/`.

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
