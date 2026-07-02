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

### Windows hardware encoders (Media Foundation)

Two Windows-only H.264 backends ship in every Windows agent, both **runtime-unvalidated** and so
never the default:

- `libretether-agent/src/mf_encoder.rs` — the **Hardware** encoder: Media Foundation with CPU-side
  colour conversion.
- `libretether-agent/src/wincap_hw.rs` — the **GPU** (zero-copy) path: DXGI → GPU BGRA→NV12 →
  Media Foundation, all on one D3D11 device, with no pixel readback.

Which encoder a session uses is **chosen by the controller, per machine** — the *Encoder* section
of a machine's detail drawer — and sent to the agent in `SessionConfig.encoder`; nothing is
persisted on the agent. The agent advertises which of `software`/`hardware`/`gpu` it can actually
run in `AgentStatus.encoders` (probed once), and the UI disables the rest. `Auto` means the agent
picks, which is **software** until the hardware paths are validated (see `build_encoder` in
`encode.rs`). A requested encoder that can't initialise falls back, and the agent reports the one it
actually used back via `SessionServer::Meta`.

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

The MFT / GPU paths can't be exercised in CI (no GPU, no runtime), so they need a manual pass on a
Windows box with a hardware H.264 encoder (any recent Intel/AMD/NVIDIA GPU). Because the code ships
in every Windows agent, **any** Windows build works — no feature flag. You pick the encoder from the
controller, which sends it to the agent per session:

1. **Pick the encoder in the app**: open the machine → its detail drawer → **Encoder**, and choose
   **Hardware** (or **GPU**). Choices the machine doesn't advertise as supported are disabled.
2. **Start a live session** and confirm the header shows **`Media Foundation (hardware)`** (or
   `… (GPU zero-copy)`) as the encoder — it comes from `ScreenEncoder::kind()` /
   `SharedConfig::report_encoder` via `SessionServer::Meta`. The agent also logs `h264 encoder: …`
   once per session on the Logs page.
3. **Watch the picture**: it must be correct (no green/pink tint = NV12 colour is right; no
   corruption/blockiness = the access units and in-band SPS/PPS are right) and must survive
   quality changes, resizes, and the periodic keyframes (which force an IDR via `ICodecAPI`).
4. **Check the win**: the per-second stats line (Debug level in the Logs page) reports
   `enc N.N (conv … sub … drn …) ms/f`. Switch the encoder between **Hardware**/**GPU** and
   **Software** in the Configure section — a change restarts the session automatically — and
   compare; the hardware/GPU `enc` figure should be markedly lower and CPU usage should drop. (The
   **Hardware** encoder still does the DXGI→CPU readback and the CPU NV12 conversion — it moves only
   the *encode* to the GPU; the **GPU** path removes the readback and conversion too.)
5. **Confirm graceful fallback**: on a guest with *no* usable encoder the requested Hardware/GPU
   encoder must fall back to OpenH264 (with a `Media Foundation unavailable … falling back` /
   `GPU zero-copy path unavailable …` log line), and the session header must show the encoder that
   actually ran.

When that holds across a few GPUs, make `Auto` prefer the hardware path in `build_encoder`
(`encode.rs`) so it becomes the default, and drop the README/ARCHITECTURE "pending validation"
notes.

## CI

- **`ci.yml`** — on every PR/push, split by whether a step's result depends on the OS:
  - a **`lint`** job (Linux, once) runs `run check:static` — the OS-independent gate: JS/TS lint,
    TypeScript type-check, `rustfmt --check`, and the license check.
  - a **`rust`** matrix (`ubuntu` / `macos` / `windows`) runs `run check:rust` (workspace clippy)
    and `run test`, so the whole workspace — including per-OS code like the Windows Media
    Foundation encoder and the macOS capture path — is compiled and tested on every platform we
    ship. `run check` runs both locally. One carve-out: the Windows leg runs `run test:headless`
    (everything minus the desktop crate) because a Tauri-linked test binary can't start outside a
    bundled app there (WebView2Loader.dll import → `STATUS_ENTRYPOINT_NOT_FOUND`); desktop tests
    still compile on Windows and run on the other two legs.
- **`release.yml`** — on a `chore: release …` commit: builds and attests the agent + relay for
  every platform, publishes the multi-arch relay container, and pins the one-line installers.

Releases are cut with `run release <major|minor|patch>`, which bumps the version across
manifests, commits, tags, and pushes.
