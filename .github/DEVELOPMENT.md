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

### Windows hardware encoder (`media-foundation` feature)

`libretether-agent/src/mf_encoder.rs` is a Windows-only, off-by-default backend (feature
`media-foundation`). Because it's `#[cfg(windows)]`, the Linux `check` job never compiles it, so
CI has a dedicated `windows-latest` job (`.github/workflows/ci.yml`) that builds it. To
compile-check it locally from Linux without a Windows box, use a rootless
[LLVM-mingw](https://github.com/mstorsjo/llvm-mingw) toolchain:

```bash
# Extract the ubuntu tarball, put its bin/ on PATH, then:
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-clang
export CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-clang++
export AR_x86_64_pc_windows_gnu=llvm-ar
export OPENH264_NO_ASM=1
cargo clippy -p libretether-agent --target x86_64-pc-windows-gnu --features media-foundation -- -D warnings
```

## CI

- **`ci.yml`** — on every PR/push: `run check` (format, lint, type-check, license) + `run test`
  on Linux, plus the Windows compile-check for the `media-foundation` feature.
- **`release.yml`** — on a `chore: release …` commit: builds and attests the agent + relay for
  every platform, publishes the multi-arch relay container, and pins the one-line installers.

Releases are cut with `run release <major|minor|patch>`, which bumps the version across
manifests, commits, tags, and pushes.
