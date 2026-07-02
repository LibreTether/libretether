# Architecture & design

Technical reference for how LibreTether works under the hood. For using it, see the
[README](../README.md); for building it, see [DEVELOPMENT.md](DEVELOPMENT.md); for running a
relay, see [RELAY.md](RELAY.md).

## Overview

LibreTether is two programs that talk over [QUIC](https://en.wikipedia.org/wiki/QUIC):

- **The controller** — the Tauri desktop app. It is the one fixed point in the system: it
  *listens*, and agents dial in to it. It manages your machines, drives control sessions, and
  holds the registry of enrolled agents and their pinned public keys.
- **The agent** (`libretether-agent`) — a small headless daemon on each controlled machine. It
  dials the controller and holds the connection open, so the machine stays reachable even
  behind NAT/firewalls (outbound connections "just work"). It performs screen capture, input
  injection, command execution, and hosts the RDP/SSH bridges.

```
 ┌──────────────┐        agent dials out + key auth        ┌────────────────────┐
 │  Controller  │ ◀─────────────────────────────────────── │  Background agent  │
 │  (this app)  │ ───────────────────────────────────────▶ │  (client machine)  │
 │   listens    │   control · exec · screenshot · input    │  systemd/launchd/… │
 └──────────────┘            live video (H.264)             └────────────────────┘
```

Once authenticated, the controller drives the agent by opening a fresh bidirectional QUIC
stream per request. Live screen control uses a single long-lived, full-duplex stream: the
controller writes input events while the agent writes control messages interleaved with
binary video frames.

## Connection modes

Each controller has a **type** that sets how it and its clients reach each other. None require
the client to log in.

- **Tailscale** — the controller is given a Tailscale auth key; deploy scripts run
  `tailscale up --authkey=…` so each client joins the tailnet non-interactively. NAT traversal
  is free (Tailscale's DERP relays). The controller listens on its tailnet address; agents dial
  in. The link is end-to-end encrypted by the tailnet on top of QUIC's TLS 1.3.
- **Direct** — no Tailscale. Agents dial the controller's advertised address, which must be
  reachable (LAN, an existing VPN, or a port-forward on the controller). Zero third-party
  dependency.
- **Relay (server-backed)** — `libretether-relay` runs on a public host; the controller **and**
  every client dial *out* to it, so nothing on either side is exposed. The relay routes between
  them and carries the control plane, live session, and tunneled RDP/SSH. Sessions **upgrade to a
  direct peer-to-peer path** whenever a NAT hole-punch succeeds (see below), so the relay carries
  only the hard-NAT minority. It's **multi-tenant** — one host serves many controllers, each a
  separate tenant (its own owner/agent secrets, isolated routing), provisioned via the relay's
  admin secret. See [RELAY.md](RELAY.md).

### Peer-to-peer NAT traversal

In relay mode the relay doubles as a **STUN-like rendezvous**: it observes each peer's reflexive
(public) address as the source of that peer's QUIC connection, and can hand each side the other's.
Both peers use a single **dual-role endpoint** — one UDP socket that dials the relay *and* accepts a
direct connection — so the NAT mapping the relay observed is exactly the one a punch reuses.

The upgrade is opportunistic and layered on top of the reliable relay path:

1. When an agent registers, the controller asks the relay to broker a punch (a `RelayRequest::Punch`).
   The relay pushes the controller's reflexive address to the agent on its (otherwise idle) hello
   stream (a `RelaySignal::Punch`) and replies to the controller with the agent's.
2. The **agent dials the controller directly** from its relay socket (which opens the agent's NAT),
   while the **controller sends a few packets toward the agent** to open its own NAT. When both NATs
   are address/port-restricted cones this meets in the middle; symmetric NAT/CGNAT doesn't, and the
   attempt simply times out.
3. The direct connection runs the **normal mutual handshake** (Ed25519 auth + the end-to-end key
   agreement), so it's exactly as trusted as Direct mode — the relay only exchanged addresses, it
   didn't grant anything. The controller then attaches it to the agent's link as an upgrade.
4. New control/session/tunnel streams **prefer the direct path**; an in-flight session finishes on
   whatever path it started on. If the direct path drops, streams **fall back to the relay**
   transparently, and the relay stays the authority for online/offline presence throughout.

The end-to-end AEAD (above) covers the direct path too, so upgrading changes only *where the bytes
flow*, never their confidentiality.

## The live video pipeline

The watch/control session streams the guest's screen to the controller as an **inter-frame
H.264 stream, decoded by WebCodecs** on the controller. The capture→encode→write stages run on
separate threads so capturing the next frame overlaps encoding the current one.

```
capture thread ──RawFrame──▶ encoder thread ──OutFrame──▶ async writer
 (DXGI/xcap/pw)  RGBA, newest-  (RGBA→I420→H.264)  bounded   (QUIC stream)
                 wins, drop stale
```

### Capture backends (per OS)

The agent picks a capture backend at runtime; the active one is reported to the controller and
shown in the session header.

| Platform | Backend | Notes |
|----------|---------|-------|
| Windows | **DXGI Desktop Duplication** | GPU-accelerated, event-driven; falls back to GDI `BitBlt` (via `xcap`) when duplication is unavailable (RDP/console-0/GPU-less) |
| Linux (X11) | **xcap** | X11 grab |
| Linux (Wayland) | **PipeWire** | via the XDG ScreenCast portal |
| macOS | **xcap** | CoreGraphics |

### Encoder backends

Encoding sits behind a `ScreenEncoder` trait so hardware encoders drop in per-OS. All backends
emit the same H.264 wire format; the choice is a runtime capability, not a protocol fallback.

- **OpenH264 (software)** — the cross-platform default and universal fallback. Baseline profile
  (widest WebCodecs decode support, including WebKitGTK), bitrate-based rate control, tuned for
  low-latency screen content.
- **Media Foundation (hardware, Windows)** — an async-MFT backend with CPU-side colour
  conversion, plus a **GPU zero-copy** variant (`wincap_hw.rs`: DXGI → GPU BGRA→NV12 → the encoder
  on one D3D11 device, no readback). Both compiled into every Windows agent but **never the
  default at runtime** while pending validation on real hardware. Which encoder a session uses is
  **chosen by the controller per machine** and sent in `SessionConfig.encoder`; the agent
  advertises the ones it can run in `AgentStatus.encoders` (nothing is persisted agent-side), falls
  back if a requested one can't start, and reports the actual encoder back in `SessionServer::Meta`.
  Compile-verified in CI; making a hardware path the `Auto` default is a one-line change in
  `build_encoder` once validated.
- **Planned:** VAAPI/NVENC on Linux and VideoToolbox on macOS (see the README's *Planned next
  steps*).

A cheap whole-frame hash short-circuits a perfectly static screen to zero bandwidth. Adaptive
mode lowers the effective resolution automatically when the encoder or link can't keep up, and
restores it as conditions clear. The controller can also retune bitrate / frame rate /
resolution live.

### Wire format & decode

Each agent→controller message is length-delimited and tagged as either a JSON control message
(`Meta`/`Error`) or a binary video frame. A video frame carries one H.264 Annex-B access unit
(keyframe = IDR with in-band SPS/PPS, or a delta P-frame). The controller forwards the access
unit straight to a WebCodecs `VideoDecoder`, deriving the codec string from the keyframe's SPS —
it never decodes pixels itself. The TypeScript decoder mirrors the Rust wire format byte for
byte.

### Input injection

- **X11 / macOS / Windows** — `enigo` (absolute, virtual-desktop coordinates; mouse positions
  are normalized 0–1 of the captured display so they survive resolution differences).
- **Wayland** — the RemoteDesktop portal (pointer/keyboard), sharing a single portal session
  with the ScreenCast capture so the user approves one "share your screen" prompt per session.

## RDP & SSH

- **RDP** — enables an RDP server on the client and launches the controller host's RDP viewer at
  the client's address. On Linux it drives **gnome-remote-desktop** (`grdctl`) with generated
  credentials (no per-connect consent prompt, sidesteps the Wayland portal); on Windows it
  enables the built-in Remote Desktop service. macOS has no built-in RDP server.
- **SSH** — opens the controller host's terminal running `ssh` to the client. If the client
  already runs an SSH server it's used as-is; otherwise the agent runs a **built-in in-process
  SSH server** (russh) bound to loopback, reached through the same authenticated tunnel, that
  accepts a single ephemeral key the controller generates per connect. So SSH works on any
  machine — including a stock Windows box — with nothing to install.

## Security model

- **Per-machine identity.** Each agent **and** controller has its own **Ed25519** keypair; the
  private seed never leaves the machine and is stored owner-only (`0600`).
- **Mutual authentication on every connection.** The controller issues a random nonce and the
  agent signs it (accepted only against the public key recorded at enrollment); the agent issues
  a nonce back and the controller signs it (accepted only against the key the agent pinned). The
  controller's key is **required** at enrollment (`--controller-key` in the deploy command) —
  there is **no trust-on-first-use**; an agent without a pinned key must be re-enrolled.
- **No trust in the network.** Because both directions are verified at the application layer, a
  man-in-the-middle on a Direct-mode port-forward — or a party holding only a tenant's owner
  secret — cannot impersonate the controller and drive an agent. After the handshake the agent
  issues a per-connection **capability token** that every control/screen/tunnel stream must
  carry, so unauthenticated streams (e.g. injected through the relay) are rejected.
- **End-to-end encryption.** On top of the transport, every post-handshake stream is wrapped in an
  application-layer AEAD (ChaCha20-Poly1305) whose key is agreed end-to-end and bound to the pinned
  Ed25519 identities. Folded into the mutual handshake is a **signed ephemeral X25519 key
  agreement** (a station-to-station AKE): each side sends an ephemeral X25519 public key and signs
  a transcript committing to *both* ephemeral keys and *both* nonces with its long-term identity,
  and verifies the peer's signature against the pinned key — so a man-in-the-middle (a malicious
  relay included) can't substitute an ephemeral key without breaking a signature it can't forge.
  The ephemeral keys give **forward secrecy**; the derived session key seals every later stream, so
  **in relay mode the relay only ever forwards ciphertext** and can't read the screen, input,
  command output, or tunneled RDP/SSH. It is always on (the same single record layer runs in Direct
  and Tailscale mode too — defence in depth over QUIC's TLS, and no "is this encrypted?" branch to
  get wrong). See [the record layer below](#end-to-end-record-layer).
- **One-time enrollment token.** Baked into the deploy script, it binds the very first
  connection, then is burned.
- **Phone pairing (SPAKE2 PAKE).** Enrollment over the browser portal is carried by a PAKE keyed
  to the short spoken code. The relay routes the two sides by a nameplate but never learns the
  code's secret half, so it can't read the enrollment bundle or machine-in-the-middle it; the
  controller key is still pinned, and a wrong code gets a single online guess before the slot is
  burned. Both ends show a matching verify phrase as a final cross-check.
- **Transport.** QUIC encrypts everything with TLS 1.3, and on a tailnet WireGuard sits underneath
  as well. Confidentiality no longer rests on the transport in relay mode — the end-to-end AEAD
  above does that — so a **trusted relay host is no longer assumed for confidentiality**. A relay is
  multi-tenant: each tenant has its own owner secret (its controller slot) and agent secret (its
  agents), with isolated routing, and provisioning a tenant is gated by the relay's admin secret.
  A tenant's owner secret is a single controller-slot credential, so treat it as sensitive for
  *availability* (a holder can deny or disrupt that tenant's service), but it grants no view into
  the session.

### End-to-end record layer

After the handshake, each stream the controller opens carries its `StreamOpen` header in the clear
(so the agent can still tell a handshake stream from a data stream and route it), then the
controller sends a random 32-byte per-stream salt. Both ends run the salt and the session key
through HKDF into a fresh pair of directional keys (controller→agent and agent→controller), so
every stream — and each direction within it — has a unique key and starts its AEAD nonce counter at
zero with no risk of reuse. Everything after the salt, **including the capability token**, is a
sequence of length-prefixed ChaCha20-Poly1305 records. The record layer (`SecureSend`/`SecureRecv`
in `libretether-protocol::e2e`) implements `AsyncRead`/`AsyncWrite`, so the framing, H.264 video,
and RDP/SSH tunnel code above it is oblivious to the encryption; a tampered, reordered, or
truncated record fails the AEAD and drops the stream (fail-closed).

## Design decisions

### No backward compatibility

LibreTether does **not** maintain backward compatibility. When a format, protocol, or interface
changes, it changes cleanly and drops the old path — no fallbacks, version-negotiation shims, or
migration scaffolding.

- **Wire protocol:** `PROTOCOL_VERSION` is bumped and mismatched peers are rejected on *both*
  ends (the agent checks the controller's version and vice versa). The controller and agents are
  released together and must be upgraded together — there is never a v(N-1) fallback. A
  compatibility fallback is a downgrade attack, so the protocol fails closed.
- **On-disk config:** if a field becomes required, the agent refuses to operate without it (with
  a clear "re-enroll / re-deploy" error) rather than silently defaulting or migrating old files.

Prefer a clean break with a clear, actionable error (re-enroll, re-deploy, upgrade) over silent
compatibility.

### Protocol versioning history

`PROTOCOL_VERSION` (in `libretether-protocol`) has evolved: v2 added mutual authentication; v3
made the version check mutual (skew fails closed on both ends); v4 added the log-fetch RPC; v5
replaced the JSON+base64 frame with a binary tile-delta format and added live quality control;
v6 replaced the per-tile baseline-JPEG video with a real inter-frame **H.264** stream decoded by
WebCodecs (and swapped JPEG quality for a target bitrate); v7 added the live capture/encoder
backend names to the session metadata; v8 made the link **end-to-end encrypted** — the handshake
now carries ephemeral X25519 keys and the mutual-auth signatures cover a transcript over both
ephemeral keys and both nonces, so every post-handshake stream is AEAD-sealed and a relay only
forwards ciphertext; v9 added **file transfer** — a `ControlRequest::Browse` directory-listing RPC
and resumable `StreamOpen::Download` / `StreamOpen::Upload` streams (a per-file manifest + chunked
bytes, with the receiver owning durability via `.part` files so a transfer resumes after a network
blip or restart).
