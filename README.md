<div align="center">

<img src="libretether-desktop/public/libretether.png" alt="LibreTether" width="120" />

# LibreTether

**Self-hosted remote desktop for your own machines — no cloud account, no ports to open.**

Enrol a machine with a one-click script and LibreTether keeps it reachable for as long as it's
on. Check its status, run a command, grab a screenshot, or take over the screen with full mouse
and keyboard — all over your own private connection, with nothing in the middle you don't run.

<br>

![License](https://img.shields.io/badge/license-AGPL--3.0-blue)
![Release](https://img.shields.io/github/v/release/LibreTether/libretether?label=release)
![CI](https://github.com/LibreTether/libretether/actions/workflows/ci.yml/badge.svg)
![Platforms](https://img.shields.io/badge/platforms-Linux%20%C2%B7%20macOS%20%C2%B7%20Windows-informational)

**Docs:** [Architecture](.github/ARCHITECTURE.md) · [Running a relay](.github/RELAY.md) · [Development](.github/DEVELOPMENT.md)

</div>

---

## Why LibreTether

- 🖥️ **Full remote control** — live screen with mouse + keyboard takeover, streamed as H.264 for
  smooth, low-latency video.
- 🔌 **Works behind NAT & firewalls** — the agent dials *out* to you and holds the connection
  open, so there's nothing to port-forward on the machine you're controlling.
- 🔒 **Yours, end to end** — no cloud service, no sign-up, no account. Every link is mutually
  authenticated with per-machine Ed25519 keys.
- 🧰 **More than a screen** — one-click **RDP** and **SSH**, remote command execution, on-demand
  screenshots, and a live logs view.
- 🪄 **Painless enrollment** — a one-line install, or a phone-friendly pairing code you can read
  out loud over the phone.
- 🐧🍎🪟 **Cross-platform** — Linux (X11 **and** Wayland), macOS, and Windows.

## How it works

LibreTether is two programs that talk over [QUIC](https://en.wikipedia.org/wiki/QUIC):

- **The controller** — the desktop app. It *listens*; agents dial in to it.
- **The agent** — a small headless service on each machine you control. It dials the controller
  and stays connected, so the machine is reachable even behind NAT.

```
 ┌──────────────┐        agent dials out + key auth        ┌────────────────────┐
 │  Controller  │ ◀─────────────────────────────────────── │  Background agent  │
 │  (this app)  │ ───────────────────────────────────────▶ │  (client machine)  │
 │   listens    │   control · exec · screenshot · input    │  systemd/launchd/… │
 └──────────────┘            live video (H.264)             └────────────────────┘
```

**Choose how your machines reach you** when you create a controller — none require the client to
log in:

- **Tailscale** — clients join your tailnet non-interactively (free NAT traversal via Tailscale).
- **Direct** — clients dial your controller's address (LAN, VPN, or a port-forward on *your* end).
- **Relay** — both ends dial *out* to a small relay you run on a public host, so nothing anywhere
  is exposed. See **[Running a relay](.github/RELAY.md)**.

The [Architecture guide](.github/ARCHITECTURE.md) covers the video pipeline, capture backends,
and the full security model.

## Get started

### 1. Get the controller

Download the desktop app for your OS from the [latest release](https://github.com/LibreTether/libretether/releases/latest),
or build it yourself — see [Development](.github/DEVELOPMENT.md).

### 2. Add a machine

In the controller: **Machines → Add machine**, name it, and pick its OS. You get a one-line
command — run it on the target machine and it installs the agent, enrols it, and starts the
background service. It shows up as **online** within seconds.

Prefer not to copy a script? Enrol straight from a release with the token the controller shows
you:

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
always pulls the matching agent build.

> **Guiding someone over the phone?** Use **Add machine → Phone install** for a short spoken
> pairing code and a browser installer — no command to paste, no keys to dictate. Needs a relay
> with the pairing portal; see [Running a relay](.github/RELAY.md#phone-friendly-install-pairing-portal).

### 3. Connect

Once a machine is online, open it and pick how to connect:

- **Live control** — the screen streams into the LibreTether window with full mouse + keyboard
  takeover. Adjust bitrate / frame rate / resolution live from the quality menu.
- **RDP** — one click into gnome-remote-desktop (Linux) or Windows Remote Desktop, opening your
  choice of RDP viewer. No per-connect consent prompt on Linux.
- **SSH** — one click opens a terminal `ssh`'d to the machine. Works even with no SSH server
  installed — the agent runs a built-in one on demand.

Set your preferred RDP viewer and terminal under **Connection → Host tools**.

## The background agent

The deploy script installs the agent as a **per-user** background service so the machine is
reachable on every boot (per-user, because screen capture and input must run inside the
graphical session):

| OS | Service | Notes |
|----|---------|-------|
| Linux | systemd **user** unit | X11 and Wayland both supported |
| macOS | **LaunchAgent** | Needs Screen Recording + Accessibility permission (granted once) |
| Windows | per-user **autostart** | Runs at logon; no elevation needed |

## Security at a glance

- Each agent **and** controller has its own **Ed25519** identity; the private seed never leaves
  the machine.
- **Every connection is mutually authenticated** — the agent verifies the controller and the
  controller verifies the agent, against keys pinned at enrollment. There is **no
  trust-on-first-use**.
- The link doesn't trust the network: even a man-in-the-middle, or someone holding only a relay
  secret, can't impersonate the controller and drive an agent.
- QUIC encrypts everything (TLS 1.3). In relay mode the relay only forwards bytes, but it *can*
  see the decrypted stream, so use a relay host you trust.

Full details — mutual handshake, capability tokens, the SPAKE2 phone-pairing flow — are in the
[Architecture guide](.github/ARCHITECTURE.md#security-model).

## Status

An early but capable build. Working today:

- ✅ Create / list / remove machines and generate per-OS deploy scripts
- ✅ Agent enrollment, always-on background service, auto-reconnect
- ✅ Live online status, uptime, remote command execution, screenshots
- ✅ **Live screen control** — H.264 video (decoded by WebCodecs) with mouse + keyboard takeover
  and live quality controls
- ✅ **Wayland support** via XDG portals (X11 too)
- ✅ **RDP** and **SSH** one-click connect (SSH needs nothing installed on the client)
- ✅ **Relay mode** + phone-friendly browser pairing
- ✅ In-app **Logs** — live controller activity plus on-demand agent logs

Video encoding is currently software H.264 (OpenH264) on all platforms, with a hardware Windows
path in progress; see below.

## Planned next steps

- **End-to-end encryption in relay mode.** Today the relay terminates QUIC/TLS on both hops and
  forwards the *decrypted* stream, so a relay host is trusted for confidentiality (mutual Ed25519
  auth and the capability token are end-to-end, but the payload is not). Wrap the session in an
  application-layer AEAD keyed to the Ed25519 identities already pinned at enrollment — a Noise
  handshake over the existing QUIC stream — so the relay only ever sees ciphertext, removing the
  "trusted relay host" assumption and matching what Tailscale's WireGuard gives its DERP path.

- **Peer-to-peer NAT traversal (STUN/TURN).** Attempt a direct UDP hole-punch before falling back
  to relaying: the relay already sees each peer's public address, so it can double as the
  signaling/STUN rendezvous, with peers upgrading to a direct QUIC path when the punch succeeds
  and staying on the relay (as TURN) when it can't (symmetric NAT/CGNAT). Most sessions would go
  direct — lower latency, no metered relay egress — while the relay carries only the hard-NAT
  minority. Pairs naturally with the E2E layer above, which keeps even the fallback path private.

- **Hardware video encoding on Windows (Media Foundation).** Finish the GPU H.264 path on
  Windows guests. A Media Foundation encoder backend is written and compiles, but it needs
  runtime validation on real hardware before it's switched on by default — today it sits behind
  an off-by-default build feature, with software OpenH264 as the automatic fallback.

- **Hardware video encoding on Linux (VAAPI & NVENC).** Offload H.264 encoding to the GPU on
  Linux guests — VAAPI for Intel/AMD, NVENC for NVIDIA — to cut CPU cost and unlock higher
  resolutions and frame rates. Software OpenH264 stays as the automatic fallback.

- **Hardware video encoding on macOS.** A VideoToolbox encoder backend for hardware H.264 on
  macOS guests, slotting into the same encoder abstraction as the Windows Media Foundation path,
  again with OpenH264 as the fallback.

- **Multi-tenant relay.** Let a single `libretether-relay` host serve several independent
  controllers, each with its own owner/agent secrets and isolated routing — so multiple people
  or teams can share one relay without ever seeing each other's machines.

- **Cloudflare connectivity.** A connection mode built on Cloudflare (e.g. Tunnel) as an
  alternative to Tailscale, giving users already in the Cloudflare ecosystem reachability without
  exposing ports or running a relay.

## License

GNU Affero General Public License v3.0 — see [LICENSE](LICENSE).

Copyright (C) 2026 LibreTether contributors. Because LibreTether runs as a network service, the
AGPL's section 13 applies: if you offer a modified version over a network, you must also offer
its source.
