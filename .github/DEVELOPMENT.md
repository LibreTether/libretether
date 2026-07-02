# Development

How to build, run, and hack on LibreTether. For how the system works, see
[ARCHITECTURE.md](ARCHITECTURE.md).

## Prerequisites

The project uses [Runfile](https://github.com/Skiley/runfile) for task running (`run <target>`);
the underlying `pnpm`/`cargo` commands work too.

```bash
run setup      # install system prerequisites (WebKitGTK, X11/PipeWire capture libs, nasm, …) + JS deps
```

`setup` installs the Tauri/WebKit prerequisites, the Linux capture libs (`libpipewire-0.3-dev`
for Wayland), and `nasm` (used by the agent's H.264 encoder — see [Cross-compiling &
toolchains](#cross-compiling--toolchains)). On macOS it triggers the Xcode command-line tools;
on other platforms, install the [Tauri prerequisites](https://tauri.app/start/prerequisites/)
manually.

## Everyday commands

```bash
run dev                  # controller with hot reload (Rust backend + Vite UI)
run build                # production controller bundle for your OS
run agent:build          # headless agent -> target/release/libretether-agent
run agent:dev run        # run the agent from source (subcommands: enroll/run/status/install)
run relay:build          # relay binary
run relay:dev run        # run the relay from source
run check                # biome + tsc + cargo fmt + clippy + license check (the CI gate)
run lint                 # auto-format & fix everything (biome + rustfmt)
run test                 # the whole Rust workspace's tests
run :list                # list every task
```

Without Runfile: `pnpm install`, then (from `libretether-desktop/`) `pnpm exec tauri dev` /
`pnpm exec tauri build`, and `cargo build -p libretether-agent --release`.

## Layout

A Cargo workspace (root `Cargo.toml`) ties the Rust crates together; the desktop app's frontend
+ Tauri shell live in `libretether-desktop/`. Each submodule also carries its own `Runfile.json`
under a namespace, so its tasks run in its own directory (e.g. `run agent:build`,
`run relay:dev run`, `run desktop:build:web`, `run protocol:check`).

```
libretether-desktop/     desktop controller app
  src/                   React UI (Vite + Tailwind v4)
  src-tauri/             controller backend — QUIC server, registry, deploy scripts, commands
libretether-protocol/    shared wire protocol, QUIC transport, Ed25519 identity, video wire format
libretether-agent/       headless agent daemon (capture, encode, input, service install, SSH/RDP)
libretether-relay/       relay server (optional, for relay mode) + embedded pairing portal
libretether-common/      shared helpers
libretether-portal/      static assets for the browser pairing portal (embedded into the relay)
```

## Cross-compiling & toolchains

The agent's live video uses **OpenH264**, which compiles its bundled C++ via `cc` (no cmake).
Its x86 SIMD kernels are built with **`nasm`**:

- Install `nasm` for the fast path (CI and `run setup` do this on x86_64).
- Without `nasm`, set **`OPENH264_NO_ASM=1`** to fall back to the slower C-only kernels. Handy
  for sandboxes/CI that lack `nasm`; not for release builds.

Releases build the agent + relay for `linux-x86_64`, `linux-aarch64`, `macos-universal`, and
`windows-x86_64` (see `.github/workflows/release.yml`). aarch64 uses no x86 asm, so `nasm` is
only needed on the x86_64 targets.

### Windows hardware encoder (Media Foundation)

`libretether-agent/src/mf_encoder.rs` is a Windows-only H.264 backend. It's **always compiled**
on the Windows target but **off by default at runtime** — selected only when
`LIBRETETHER_ENCODER=hardware` is set — because it hasn't yet been validated against a real
hardware encoder (see the module docs). The single `DEFAULT_ENCODER_PREF` constant in `encode.rs`
is what makes it opt-in; flip it to `Hardware` once validated and it becomes the default.

Because it's `#[cfg(windows)]`, the Linux runner never compiles it — the Windows leg of the CI
`rust` matrix (`.github/workflows/ci.yml`) clippies and tests the whole workspace on
`windows-latest`, which is what keeps it from rotting. To compile-check it locally from Linux
without a Windows box, use a rootless [LLVM-mingw](https://github.com/mstorsjo/llvm-mingw)
toolchain:

```bash
# Extract the ubuntu tarball, put its bin/ on PATH, then:
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-clang
export CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-clang++
export AR_x86_64_pc_windows_gnu=llvm-ar
export OPENH264_NO_ASM=1
cargo clippy -p libretether-agent --target x86_64-pc-windows-gnu -- -D warnings
```

The pure parts of the encoder — the RGBA→NV12 conversion and the leading-SPS check —
live in `encode.rs` (not the Windows-only module) so the Linux `run test` job actually
exercises them (`nv12_is_i420_with_interleaved_chroma`, `starts_with_sps_*`); only the
Media Foundation COM plumbing is Windows-only and unit-test-free.

#### Validating it on a real Windows guest

The MFT path can't be exercised in CI (no GPU, no runtime), so it needs a manual pass on a
Windows box with a hardware H.264 encoder (any recent Intel/AMD/NVIDIA GPU). Because the code
ships in every Windows agent, **any** Windows build works — no special feature flag — and you
drive backend selection at runtime with the **`LIBRETETHER_ENCODER`** env var:

- `LIBRETETHER_ENCODER=hardware` — use Media Foundation (falls back to software, loudly, if it
  can't initialise).
- `LIBRETETHER_ENCODER=software` — force OpenH264 (the A/B baseline / a kill switch).
- unset / `auto` — the current default, which is **software** until this is validated.

1. **Run a Windows agent** with the env var set (a released build, or a local one):
   ```powershell
   $env:LIBRETETHER_ENCODER = "hardware"
   .\libretether-agent.exe run --config <path>   # or set it for the installed service
   ```
2. **Start a live session** from the controller and confirm the header shows
   **`Media Foundation (hardware)`** as the encoder (it comes from `ScreenEncoder::kind()`
   via `SessionServer::Meta`). The agent also logs `h264 encoder: Media Foundation (hardware)`
   once per session on the Logs page.
3. **Watch the picture**: it must be correct (no green/pink tint = NV12 colour is right; no
   corruption/blockiness = the access units and in-band SPS/PPS are right) and must survive
   quality changes, resizes, and the periodic keyframes (which force an IDR via `ICodecAPI`).
4. **Check the win**: the per-second stats line (Debug level in the Logs page) reports
   `enc N.N ms/f`. Toggle `LIBRETETHER_ENCODER` between `hardware` and `software`, reconnect,
   and compare — the hardware `enc` figure should be markedly lower and CPU usage should
   drop. (Note this still does the DXGI→CPU readback and the CPU NV12 conversion; it moves the
   *encode* to the GPU, not yet the whole pipeline — see the module docs.)
5. **Confirm graceful fallback**: on a guest with *no* usable encoder MFT the session must
   still work on OpenH264, with a `Media Foundation unavailable … falling back` log line.

If all of that holds across a few GPUs, flip `DEFAULT_ENCODER_PREF` to `Hardware` in `encode.rs`
(so it's on by default) and drop the README/ARCHITECTURE "pending validation" notes.

## CI

- **`ci.yml`** — on every PR/push, split by whether a step's result depends on the OS:
  - a **`lint`** job (Linux, once) runs `run check:static` — the OS-independent gate: JS/TS lint,
    TypeScript type-check, `rustfmt --check`, and the license check.
  - a **`rust`** matrix (`ubuntu` / `macos` / `windows`) runs `run check:rust` (workspace clippy)
    and `run test`, so the whole workspace — including per-OS code like the Windows Media
    Foundation encoder and the macOS capture path — is compiled and tested on every platform we
    ship. `run check` runs both locally.
- **`release.yml`** — on a `chore: release …` commit: builds and attests the agent + relay for
  every platform, publishes the multi-arch relay container, and pins the one-line installers.

Releases are cut with `run release <major|minor|patch>`, which bumps the version across
manifests, commits, tags, and pushes.
