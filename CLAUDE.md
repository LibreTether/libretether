# LibreTether — project conventions

## No backward compatibility

This project does **not** maintain backward compatibility. When a format, protocol, or
interface changes, change it cleanly and drop the old path — do **not** add fallbacks,
version-negotiation shims, or migration scaffolding to keep old clients or configs working.

- **Wire protocol:** bump `PROTOCOL_VERSION` and reject mismatched peers; never add a v1
  fallback. The controller and agents are released together and must be upgraded together.
- **On-disk config:** if a field becomes required, refuse to operate without it (with a
  clear "re-enroll / re-deploy" error) rather than silently defaulting or migrating old
  files. Example: the agent requires a pinned `controller_key` — there is no
  trust-on-first-use; an agent without one must be re-enrolled.
- **Security especially:** a compatibility fallback is a downgrade attack. Fail closed.

Prefer a clean break with a clear, actionable error (re-enroll, re-deploy, upgrade) over
silent compatibility.
