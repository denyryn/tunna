# tunna

One small Rust binary that exposes services running on your machine through a
Cloudflare tunnel. It carries out two jobs at once:

- a reverse proxy (default port `8088`) that routes requests by hostname:
  `app.example.com` goes to `localhost:3000`, `grafana.example.com` goes to
  `localhost:9000`, and so on
- a tiny web UI and API (default `127.0.0.1:8080`) where you add, disable, or
  remove those routes at runtime

The Cloudflare `cloudflared` connector stays responsible for the tunnel
itself; tunna handles everything behind it, and all state is one JSON route
file.

```
internet → Cloudflare edge → cloudflared → tunna :8088 → localhost:PORT
                                                   ↑ web UI :8080
```

## Contents

- [Requirements](#requirements)
- [1. Create the tunnel](#1-create-the-tunnel)
- [2. Install tunna](#2-install-tunna)
- [3. Configure](#3-configure)
- [4. Try it without systemd](#4-try-it-without-systemd)
- [5. Run it as a service](#5-run-it-as-a-service)
- [6. Expose a port](#6-expose-a-port)
- [Where routes live](#where-routes-live)
- [Good to know](#good-to-know)

## Requirements

- Linux (the release binary is built for x86_64; build from source for others)
- A domain on Cloudflare
- `cloudflared`, the official connector binary, installed alongside tunna

## 1. Create the tunnel

In the Cloudflare dashboard: **Zero Trust → Networks → Tunnels → Create
tunnel**. Choose Cloudflared, name it, and copy the token (shown under
"Install connector"; you only need the long `eyJ...` string, not their install
commands).

While adding a public hostname to the tunnel, set:

| Field      | Value                    |
| ---------- | ------------------------ |
| Subdomain  | `*`                      |
| Domain     | your domain              |
| Service    | `http://localhost:8088`  |

Also create the wildcard DNS route `*.yourdomain.com → <tunnel-id>.cfargotunnel.com`
(the dashboard offers this while you add the hostname). One rule covers every
port you will ever expose; tunna's proxy does the per-hostname routing.

## 2. Install tunna

Download a release, or build it yourself:

```sh
# from a release
tar -xzf tunna-x86_64-linux.tar.gz
install -m755 dist/tunna /usr/local/bin/

# or from source
cargo install --git https://github.com/denyryn/tunna
```

Install `cloudflared` from
[Cloudflare's releases](https://github.com/cloudflare/cloudflared/releases)
(a `.rpm`, `.deb`, or a static binary; Fedora also gets it as an `.rpm`
directly from the latest release page).

## 3. Configure

Everything tunna reads comes from environment variables, with working
defaults except the token:

| Variable       | Default          | Meaning                                  |
| -------------- | ---------------- | ---------------------------------------- |
| `TUNNEL_TOKEN` | (none)           | the token from step 1, read by cloudflared |
| `BASE_DOMAIN`  | `example.com`    | used to default route hostnames           |
| `LISTEN_PORT`  | `8088`           | what the tunnel connector points at        |
| `MANAGER_BIND` | `127.0.0.1`      | where the web UI listens                  |
| `MANAGER_PORT` | `8080`           | web UI port                               |
| `DATA_DIR`     | `./data`         | houses `routes.json`                      |

Put them in a `.env` file in the project directory, or the ones systemd needs
directly into the unit files (step 5). A sensible `.env`:

```sh
TUNNEL_TOKEN=eyJ....
BASE_DOMAIN=example.com
LISTEN_PORT=8088
MANAGER_PORT=8080
```

## 4. Try it without systemd

From a directory containing your `.env` and where `./data` can be created:

```sh
# terminal 1
source .env && cloudflared tunnel --no-autoupdate run --token "$TUNNEL_TOKEN"

# terminal 2
source .env && tunna
```

You should see `proxy listening on 0.0.0.0:8088 (base domain example.com)` and
`manager listening on 127.0.0.1:8080`.

## 5. Run it as a service

Two units ship in this repository: `tunna.service` (the binary) and
`tunnel-cloudflared.service` (the connector; it waits for tunna and restarts
with it).

They reference absolute paths, so adjust `WorkingDirectory` and `ExecStart`
to wherever you keep them, then:

```sh
sudo cp tunna.service tunnel-cloudflared.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now tunna.service tunnel-cloudflared.service
sudo systemctl status tunnel-cloudflared.service   # expect "connected" in the log
```

If your token lives in a `.env` file instead of being spelled out in the unit,
`tunnel-cloudflared.service` already does `EnvironmentFile=path/to/.env`,
so point that line at yours and remove the absolute-paths guesswork.

## 6. Expose a port

Open `http://127.0.0.1:8080`:

1. Type a port number, e.g. `3000`
2. Optionally type a hostname; empty means `p3000.example.com`
3. Click **Expose port**

The route is live in a second or two. The list below shows every route with a
switch to turn it off without deleting it, and a remove button that deletes
it from the table and from memory at once.

You can also drive it from the API instead of the UI:

```sh
curl -X POST localhost:8080/api/routes -d '{"port": 3000}'
curl localhost:8080/api/routes
curl -X PATCH localhost:8080/api/routes/p3000.example.com -d '{"enabled": false}'
curl -X DELETE localhost:8080/api/routes/p3000.example.com
```

## Where routes live

`$DATA_DIR/routes.json`. Every change is written here before tunna responds,
so the file is the source of truth across restarts:

```json
[
  { "host": "p3000.example.com", "port": 3000, "enabled": true }
]
```

`enabled` defaults to true when absent, and the file format never changed
since the first version of this project, which was a Python equivalent —
upgrade without touching your data.

## Good to know

- The manager UI has no login. It binds `127.0.0.1` by default and nothing on
  the internet can reach it directly. Do not expose it through a tunnel route
  unless you front it with Cloudflare Access: anything on the tunnel is
  public otherwise.
- A request for a hostname that is not in the table gets a plain `404 no
  tunnel route for this host`. A route pointing at a dead port gets `502`.
- Hostnames are validated: lowercase letters, digits, dashes, at least two
  dot-separated labels. Rejecting garbage happens at the API, not at the
  proxy.
- Route changes are visible immediately, no reload, no restart.
- Requests and responses stream through unbuffered; a WebSocket client to an
  exposed port goes through the tunnel as a two-way pipe.
