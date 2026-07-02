# Running a relay

`libretether-relay` is an optional server you run on a public host so that **neither** the
controller nor the clients need to be reachable — both dial *out* to the relay, which routes
between them. It carries the control plane, the live session, and tunneled RDP/SSH. Where the
network allows, sessions **upgrade to a direct peer-to-peer path** (a NAT hole-punch the relay
brokers), so most traffic goes straight between the two ends — lower latency and no metered relay
egress — and the relay carries only the hard-NAT minority. Use it for fleets where neither end is
exposed. For the security model and how the punch works, see
[ARCHITECTURE.md](ARCHITECTURE.md#security-model).

A relay is **multi-tenant**: one host can serve several independent controllers, each with its
own **owner secret** (its controller) and **agent secret** (its agents) and its own isolated
routing — so multiple people or teams can share one relay without ever seeing each other's
machines. Provisioning a tenant is gated by the relay's single **admin secret** (or open to
anyone, if the operator enables open registration). See [Tenants](#tenants) below.

Authentication is layered: a tenant's two secrets gate access to that tenant, and the agent still
proves its identity to the controller end-to-end with Ed25519. The session is also **end-to-end
encrypted** — the controller and agent agree an application-layer key bound to their pinned
identities, so the relay only ever forwards **ciphertext** and never sees your screen, input,
command output, or tunneled RDP/SSH. It still needs to be available and trusted not to *disrupt*
service, but it is not trusted with the contents of a session.

## Quick start

1. On a cloud host with a public IP, get `libretether-relay` (grab
   `libretether-relay-linux-x86_64` from a release, or `run relay:build`) and run it. First run
   generates a config and mints a single **admin secret** (which gates provisioning tenants):
   ```bash
   libretether-relay run
   libretether-relay info       # reprint the listen address + the admin secret + a tenant summary
   ```
2. In the app's launch screen: **New controller → Relay**, name it, enter the relay's
   `host:port`, keep **Tenant → Provision a new tenant**, paste the **admin secret**, then
   **Provision & create**. The app mints a tenant on the relay (its own owner + agent secrets)
   and saves this controller with them; **Connect** dials the relay instead of listening.
3. Add machines as usual — their deploy scripts embed *this tenant's* agent secret and enrol
   against the relay (no Tailscale, no exposure). Open **UDP 47600** on the host's firewall
   (QUIC is UDP).

## Tenants

Each tenant is fully isolated: a controller only ever sees the agents that dialed in under its
own tenant's agent secret, and the relay's Logs view a controller pulls is scoped to its tenant.
There are two ways to create one:

- **From the app (a running relay).** *New controller → Relay → Provision a new tenant*, with the
  admin secret. The tenant is minted live and persisted to the relay's config. To add a **second
  device** to a tenant you already own, choose *Use existing tenant secrets* instead and paste
  that tenant's owner + agent secrets (from when it was provisioned).
- **From the shell.** Operate directly on the config (takes effect on the relay's next restart):
  ```bash
  libretether-relay tenant add --name "Team A"   # prints the tenant id + owner/agent secrets
  libretether-relay tenant list                  # ids + names, no secrets
  libretether-relay tenant rm <tenant-id>
  ```

**Open registration.** By default only the admin-secret holder can provision. To let anyone who
can reach the relay mint their *own* tenant (they still can't list or revoke others'), set
`"open_registration": true` in the config and restart. Provision-only clients then leave the
admin-secret field blank in the app.

## With Docker

A multi-arch image (`linux/amd64`, `linux/arm64`) is published to GHCR on every release:

```bash
# Generate config + print the admin secret (one-time), then run the relay.
docker run --rm -v libretether:/data ghcr.io/libretether/libretether-relay:latest info
docker run -d --name libretether-relay -p 47600:47600/udp \
  -v libretether:/data --restart unless-stopped \
  ghcr.io/libretether/libretether-relay:latest
```

The named volume (`/data`) keeps the generated config — the admin secret, every tenant's
owner/agent secrets, and the TLS cert — stable across restarts. Build it yourself with
`run relay:docker:build` (or `docker build -t libretether-relay .`).

To reprint the admin secret from an **already-running** container, pass `--config` explicitly
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
