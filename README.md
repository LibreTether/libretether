<div align="center">

<img src="public/tether.svg" alt="Tether" width="120" />

# Tether

**A self-hosted remote desktop controller that reaches your machines over your own private mesh.**

Enrol a machine with a one-click script, and Tether keeps it reachable for as long as
it's on — check its status, run commands, take screenshots, and take over the screen
with full mouse and keyboard control. No cloud service in the middle, no account to
sign up for.

</div>

---

## How it works

Tether is two programs that talk over [QUIC](https://en.wikipedia.org/wiki/QUIC):

- **The controller** — this Tauri desktop app. It manages your machines and is the one
  fixed point in the system: it *listens*, and agents dial in to it.
- **The agent** (`tether-agent`) — a small headless daemon that runs on each machine you
  want to control. It dials the controller and holds the connection open, so the machine
  stays reachable even behind NAT/firewalls (outbound connections "just work").

```
 ┌──────────────┐        agent dials out + key auth        ┌────────────────────┐
 │  Controller  │ ◀─────────────────────────────────────── │  Background agent  │
 │  (this app)  │ ───────────────────────────────────────▶ │  (client machine)  │
 │   listens    │   control · exec · screenshot · input    │  systemd/launchd/… │
 └──────────────┘            live screen frames             └────────────────────┘
```

### Reaching machines without a cloud server

Tether has no rendezvous/relay server of its own. On the **Controller** page you set the
address agents dial and pick how they connect — neither mode needs the client to log in:

- **Tailscale (pre-auth key)** — paste a [Tailscale **auth key**](https://tailscale.com/kb/1085/auth-keys)
  (generated once in your admin console) into the controller. Deploy scripts run
  `tailscale up --authkey=…`, so each client joins your tailnet **non-interactively — no SSO
  on the client**. You get NAT traversal for free (Tailscale's DERP relays, not infra you
  run). Keys can be reusable, ephemeral, and tagged.
- **Direct** — leave the auth key blank. The agent just dials the advertise address, which
  must be reachable (same LAN, an existing VPN, or a port-forward on the controller). Zero
  third-party dependency.

The controller is the one fixed point either way: it listens, and agents dial in. A
fully self-hosted middle ground (your own coordination server) is also possible with
[Headscale](https://github.com/juanfont/headscale), at the cost of hosting it yourself.

### The background agent

The deploy script installs `tether-agent` as a **per-user** background service so the
machine is reachable on every boot. Per-user (not a system daemon) matters because screen
capture and input injection must run inside the graphical session:

| OS | Service | Notes |
|----|---------|-------|
| Linux | systemd **user** unit (`tether-agent.service`) | Needs the graphical session; X11 and Wayland are both supported (see below) |
| macOS | **LaunchAgent** (`com.tether.agent`) | Requires Screen Recording + Accessibility permissions (granted once in System Settings) |
| Windows | logon **scheduled task** (`TetherAgent`) | Runs in the interactive console session |

### X11 and Wayland

The agent detects the session at runtime and picks a backend:

- **X11** — capture via `xcap`, input via `enigo`. Works out of the box.
- **Wayland** — capture via the **ScreenCast** portal (a PipeWire stream), input via the
  **RemoteDesktop** portal, and one-shot screenshots via the **Screenshot** portal. Both
  live portals share a single session, so the user approves **one** "share your screen"
  prompt per session. Building the live-capture path needs `libpipewire-0.3-dev` (installed
  by `run setup`); `run build:agent` enables it via the `wayland-capture` feature. Without
  that feature the agent still does Wayland input + screenshots, just no live frames.

### Two ways to take control

- **Live control (in-app)** — the controller streams frames and injects input over its own
  QUIC session, rendered inside the Tether window. On Wayland this uses the portals (one
  consent prompt per connect).
- **RDP** — the **Connect via RDP** button enables an RDP server on the client and launches
  your host's RDP viewer pointed at the client's tailnet IP. On Linux it drives
  **gnome-remote-desktop** (`grdctl`) with generated credentials, so there's **no per-connect
  consent prompt** and it sidesteps the Wayland portal entirely; on Windows it enables the
  built-in Remote Desktop service (sign in with the PC's Windows account). Requirements: an
  RDP client on the **controller** (FreeRDP — `freerdp3-x11`, installed by `run setup`) and
  `gnome-remote-desktop` on **Linux clients** (installed by the deploy script). macOS has no
  built-in RDP server. The connection rides Tailscale straight to the client's private IP —
  no extra tunneling.

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
- ✅ **RDP connect** — one-click into gnome-remote-desktop / Windows RDP over the tailnet

Rough edges & next up: frame streaming is JPEG-over-QUIC (no delta/codec yet), input
mapping is tuned for the primary display, the agent binary isn't published as a release
yet (the deploy script takes it from `TETHER_AGENT_BIN`/`TETHER_AGENT_URL`), and the
Wayland PipeWire capture (`src-tauri/agent/src/pwstream.rs`) is the newest piece and
benefits from testing across compositors.

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
run build:agent     # -> src-tauri/target/release/tether-agent
```

> Without Runfile: `pnpm install`, then `pnpm exec tauri dev` / `pnpm exec tauri build`,
> and `cargo build --manifest-path src-tauri/Cargo.toml -p tether-agent --release`.

### Enrolling a machine

1. In the controller, open **Machines → Add machine**, name it and pick its OS.
2. Copy or save the generated deploy script and run it on the target machine. Point it at
   the agent binary first:

   ```bash
   # On the client machine, with the built agent next to you:
   TETHER_AGENT_BIN=/path/to/tether-agent bash tether-deploy-<name>.sh
   ```

3. The script joins the machine to Tailscale, installs the agent, enrols it, and starts
   the background service. It shows up as **online** in the controller within seconds.

> Trying it on one machine? You can skip the script: run
> `tether-agent enroll --controller <addr> --token <token>` then `tether-agent run`
> (or `tether-agent install`) by hand.

## Development

```bash
run dev           # controller with hot reload
run dev:agent -- run     # run the agent from source (subcommands: enroll/run/status/install)
run check         # biome + tsc + cargo fmt + clippy (the CI gate)
run lint          # auto-format & fix everything
```

### Layout

```
src/                     React UI (Vite + Tailwind v4)
src-tauri/               Cargo workspace
  src/                   controller app — QUIC server, registry, deploy scripts, commands
  protocol/              shared wire protocol, QUIC transport, Ed25519 identity
  agent/                 headless tether-agent daemon (capture, input, service install)
```

## License

MIT — see [LICENSE](LICENSE).
