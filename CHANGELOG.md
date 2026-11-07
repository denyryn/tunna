# Changelog

All notable changes to tunna are documented here (Keep a Changelog, semver).

## 0.2.0 - 2026-11-07

### Added

- One-line install: `install.sh` downloads the release tarball, verifies the
  embedded checksum, installs the binary and `cloudflared`, generates both
  systemd units from your paths, and creates the env file without overwriting
  an existing one.
- `tunna-tray`, a desktop tray companion (in `tray/`): tray icon with menu,
  click toggles a compact popup to read status, toggle, add and remove routes
  through the manager API; close hides it, X-GNOME autostart on install.
  `install.sh` now installs both binaries; releases ship
  `tunna-tray-x86_64-linux.tar.gz` alongside the server tarball (skip the tray
  with `TUNNA_SKIP_TRAY=1`).

### Changed

- The web manager UI (`/` on `127.0.0.1:8080`) is redesigned and aligned with
  the tray popup: same tokens, faces and components. Same API, same
  `routes.json` schema, no migration.

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
