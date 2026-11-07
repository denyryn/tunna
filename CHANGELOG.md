# Changelog

All notable changes to tunna are documented here (Keep a Changelog, semver).

## 0.1.0 - 2026-11-07

Initial release.

- Single Rust binary: host-based reverse proxy on `LISTEN_PORT` and manager
  UI/API on `MANAGER_PORT` — replaces the previous Caddy + Python manager +
  `docker` stack.
- Same `data/routes.json` schema (`{"host", "port", "enabled"}`): existing
  route tables carry over unchanged.
- Manager API matches the Python manager exactly: `GET/POST/PATCH/DELETE
  /api/routes`, same status codes, same error strings, same
  `p{port}.{BASE_DOMAIN}` default hostname.
- Proxy fallback: unknown host → `404 no tunnel route for this host`,
  dead backend → `502`.
- WebSocket upgrades (HTTP/1.1, bidirectional pipe).
- Web UI embedded in the binary (`include_str!`).
- systemd units included: `tunna.service`, `tunnel-cloudflared.service`
  (cloudflared connector, token from `.env`.
