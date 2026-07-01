# Running a relay

`libretether-relay` is an optional server you run on a public host so that **neither** the
controller nor the clients need to be reachable — both dial *out* to the relay, which routes
between them. It carries everything: the control plane, the live session, and tunneled RDP/SSH.
Use it for fleets where neither end is exposed. For the security model, see
[ARCHITECTURE.md](ARCHITECTURE.md#security-model).

Authentication is layered: two relay **secrets** gate access to the relay, and the agent still
proves its identity to the controller end-to-end with Ed25519 — the relay only forwards bytes.

## Quick start

1. On a cloud host with a public IP, get `libretether-relay` (grab
   `libretether-relay-linux-x86_64` from a release, or `run relay:build`) and run it. First run
   generates a config and prints an **owner secret** and an **agent secret**:
   ```bash
   libretether-relay run
   libretether-relay info       # reprint the listen address + the two secrets
   ```
2. In the app's launch screen: **New controller → Relay**, name it, enter the relay's
   `host:port` and the two secrets, then **Connect**. The controller dials the relay instead of
   listening.
3. Add machines as usual — their deploy scripts enrol against the relay (no Tailscale, no
   exposure). Open **UDP 47600** on the host's firewall (QUIC is UDP).

## With Docker

A multi-arch image (`linux/amd64`, `linux/arm64`) is published to GHCR on every release:

```bash
# Generate config + print the secrets (one-time), then run the relay.
docker run --rm -v libretether:/data ghcr.io/libretether/libretether-relay:latest info
docker run -d --name libretether-relay -p 47600:47600/udp \
  -v libretether:/data --restart unless-stopped \
  ghcr.io/libretether/libretether-relay:latest
```

The named volume (`/data`) keeps the generated config — the owner/agent secrets and the TLS
cert — stable across restarts. Build it yourself with `run relay:docker:build` (or
`docker build -t libretether-relay .`).

To reprint the secrets from an **already-running** container, pass `--config` explicitly
(`docker exec` bypasses the entrypoint that normally supplies it):

```bash
docker exec libretether-relay libretether-relay --config /data/config.json info
```

## Phone-friendly install (pairing portal)

For someone you're guiding over the phone — who can't paste a command — a relay can also serve a
**browser pairing portal**. In the app, **Add machine → Phone install** mints a short code; you
read out *"go to `your-relay`, code `4F9K-2A7C`"*. They open the site, type the code, and
download a one-click installer — no command, no keys to dictate.

Under the hood this is a [SPAKE2](https://datatracker.ietf.org/doc/html/draft-irtf-cfrg-spake2)
PAKE brokered by the relay (`libretether-portal`, embedded in the relay binary): the code splits
into a nameplate the relay routes on and a password it never sees, so the relay can't read the
enrollment bundle or machine-in-the-middle it, and a wrong code gets a single online guess
before the slot is burned. The controller pins its key exactly as the pasted flow does — no
trust-on-first-use. Both ends show a matching verify phrase as a final cross-check.

### Serving the portal over HTTPS

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

The relay obtains and renews the cert itself (caching it in `/data/acme`) and serves the portal
on the public 443, redirecting 80→443. The ACME client is `rustls-acme` pinned to *ring* — no
extra build tooling.

**Other TLS options** (if you'd rather not use the relay's ACME):

- **Your own certificate** — set `tls_cert_path`/`tls_key_path` (PEM) in the `portal` config
  block and the relay terminates TLS itself.
- **A reverse proxy** — set only `LIBRETETHER_PORTAL_DOMAIN`, leave ACME off, and run the relay
  behind Caddy/Traefik/nginx (which do ACME for you), pointed at the plain-HTTP port.

### Config file

Everything the env vars set can also live in the relay config (`/data/config.json`) under a
`portal` block. `LIBRETETHER_PORTAL_DOMAIN` overrides `portal.domain` when both are set:

```jsonc
"portal": {
  "domain": "relay.example.com",
  "tls_cert_path": "/data/fullchain.pem", // optional: relay terminates TLS itself…
  "tls_key_path": "/data/privkey.pem"     // …omit both to serve plain HTTP behind a TLS proxy
}
```
