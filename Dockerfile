# syntax=docker/dockerfile:1

# ---- build -------------------------------------------------------------------
# Pinned to the workspace toolchain (rust-toolchain.toml). The full image (not
# -slim) ships a C compiler, which `ring` needs to build the QUIC/TLS stack.
FROM rust:1.94.1-bookworm AS build
WORKDIR /build

# Only the Cargo workspace is needed: the relay (`libretether-relay`) is a pure-Rust
# QUIC server with no system dependencies, and `cargo build -p libretether-relay`
# compiles just it, `libretether-protocol`, and `libretether-common` — never the
# Tauri controller crate. Cargo still has to *load* every workspace member, so each
# member manifest must be present; the desktop crate's source is never compiled (we
# only build -p relay).
COPY Cargo.toml Cargo.lock ./
COPY libretether-protocol ./libretether-protocol
COPY libretether-common ./libretether-common
COPY libretether-agent ./libretether-agent
COPY libretether-relay ./libretether-relay
COPY libretether-desktop/src-tauri ./libretether-desktop/src-tauri
# The relay embeds the pairing portal SPA at compile time (include_dir!), so the
# static page must be in the build context.
COPY libretether-portal/static ./libretether-portal/static
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build -p libretether-relay --release \
 && cp target/release/libretether-relay /usr/local/bin/libretether-relay

# ---- runtime -----------------------------------------------------------------
FROM debian:bookworm-slim AS runtime
LABEL org.opencontainers.image.title="libretether-relay"
LABEL org.opencontainers.image.description="LibreTether relay — routes QUIC streams between a controller and its agents"

# Unprivileged user. /data holds the generated config (owner/agent secrets + TLS
# cert); mount a volume there so the secrets survive restarts.
RUN useradd --system --uid 10001 --user-group --no-create-home libretether \
 && mkdir -p /data && chown libretether:libretether /data
COPY --from=build /usr/local/bin/libretether-relay /usr/local/bin/libretether-relay

USER libretether
VOLUME ["/data"]
# QUIC runs over UDP. The optional browser pairing portal (when configured) serves
# HTTP/HTTPS over TCP — publish 80/443 too if you enable it.
EXPOSE 47600/udp
EXPOSE 80/tcp
EXPOSE 443/tcp

# `--config` is a global flag, so it applies to both `run` (the default below)
# and `info`. Override the command, e.g. `docker run … info`, to read secrets.
ENTRYPOINT ["libretether-relay", "--config", "/data/config.json"]
CMD ["run"]
