# tunna-tray

Tauri v2 desktop tray app for the [tunna](../tunna) headless Rust server.

Lives in the system tray. Manages tunna's local manager API (127.0.0.1:8080):
add/remove/enable/toggle port routes. The live server sends no CORS headers,
so the popup UI calls it via `tauri-plugin-http` (`window.__TAURI__.http.fetch`),
not plain `fetch`.

## Build

```sh
cargo build
```

## Run

```sh
cargo run
```

## Tray behavior

- **Left-click** tray icon: show/toggle popup (400x540, positioned near tray).
- **Right-click** menu:
  - Open tunna popup
  - Open manager UI (localhost:8080) — default browser
  - Quit — actually exits
- Window close (X) hides the popup instead of exiting.
- Popup is always-on-top, skips taskbar, non-resizable.

## Live server

The tunna server runs as a systemd service and is not restarted by this app.
Only the manager API is touched: `GET/POST /api/routes`, `PATCH/DELETE /api/routes/{host}`.

## UI note

`ui/index.html` is an expendable placeholder ("loading") — safe to rewrite.
