# libretether-portal

The browser page a not-yet-enrolled machine opens to pair from a phone-friendly
flow: the operator reads a short code aloud (see
[`libretether_protocol::pairing`](../libretether-protocol/src/pairing.rs)), the
person types it here, picks their OS, and downloads a one-click installer that runs
`libretether-agent pair` with the code.

## No build step

`static/index.html` is a single self-contained static page (inline CSS + vanilla JS)
— there is intentionally **no bundler or npm install**, so it's not a build artifact
(the name is `static/`, not `dist/`, so it isn't swept up by the repo's `dist`
gitignore). The relay embeds `static/` into its binary (`include_dir!` in
`libretether-relay/src/portal.rs`) and serves it, so the relay stays a single
deployable.

Edit `static/index.html` directly. The script-generation function (`buildInstaller`)
is exposed on `window` so it can be exercised from a browser/preview.

## How it's served

The relay serves this over HTTP(S) when the portal is enabled (a `portal` block in
the config, or `LIBRETETHER_PORTAL_*` env vars). Put TLS in front — the relay's own
ACME (`LIBRETETHER_PORTAL_ACME=1`), a cert it loads directly, or a reverse proxy — so
the page's integrity is protected. The actual pairing secret never touches the page
or the relay; it rides the PAKE over QUIC.
