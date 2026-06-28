# syntax=docker/dockerfile:1

# ---- build -------------------------------------------------------------------
# Pinned to the workspace toolchain (rust-toolchain.toml). The full image (not
# -slim) ships a C compiler, which `ring` needs to build the QUIC/TLS stack.
FROM rust:1.94.1-bookworm AS build
WORKDIR /build

# Only the Cargo workspace is needed: the relay (`tether-server`) is a pure-Rust
# QUIC server with no system dependencies, and `cargo build -p tether-server`
# compiles just it and `tether-protocol` — never the Tauri controller crate.
COPY src-tauri ./src-tauri
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/src-tauri/target \
    cargo build --manifest-path src-tauri/Cargo.toml -p tether-server --release \
 && cp src-tauri/target/release/tether-server /usr/local/bin/tether-server

# ---- runtime -----------------------------------------------------------------
FROM debian:bookworm-slim AS runtime
LABEL org.opencontainers.image.title="tether-server"
LABEL org.opencontainers.image.description="Tether relay — routes QUIC streams between a controller and its agents"

# Unprivileged user. /data holds the generated config (owner/agent secrets + TLS
# cert); mount a volume there so the secrets survive restarts.
RUN useradd --system --uid 10001 --user-group --no-create-home tether \
 && mkdir -p /data && chown tether:tether /data
COPY --from=build /usr/local/bin/tether-server /usr/local/bin/tether-server

USER tether
VOLUME ["/data"]
# QUIC runs over UDP.
EXPOSE 47600/udp

# `--config` is a global flag, so it applies to both `run` (the default below)
# and `info`. Override the command, e.g. `docker run … info`, to read secrets.
ENTRYPOINT ["tether-server", "--config", "/data/config.json"]
CMD ["run"]
