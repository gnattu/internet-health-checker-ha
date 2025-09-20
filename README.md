# Internet Health Checker (Home Assistant Add-on)

A minimal Home Assistant add-on (Rust) that periodically probes Internet health and exposes REST endpoints for Home Assistant. Supports multiple WAN links with SOCKS5 per-link routing and returns a human-readable status and a numeric score.

## Status & Score
- Score 0–100 composed of availability (204 success ratio), latency, and jitter.
- Normal ≥ 80, Degraded 40–79, Failed < 40.
- Only HTTP 204 counts as success; any other response or timeout counts as failure.

## Local Development
- Run service: `cd service && PORT=8000 cargo run`
- Health: `GET http://localhost:8000/healthz`
- Per-link: `GET http://localhost:8000/status/<name>`
- Scoreboard: `GET http://localhost:8000/scoreboard`

Env vars (defaults):
- `INTERVAL_SECS` (30), `REQUEST_TIMEOUT_MS` (5000), `DEGRADED_LATENCY_MS` (750), `WINDOW_SIZE` (10)
- `TARGETS_JSON` (HTTP targets array)
- `LINKS_JSON` (per-link config array: `{name, socks5h?, weights?, latency_baseline_ms?}`)

DNS & proxies:
- Use `socks5h://` in `socks5` URLs to resolve hostnames on the proxy (per‑WAN DNS). Using `socks5://` would resolve locally, which can leak DNS through the default route.

## Home Assistant Integration (RESTful Sensor)
You can use MQTT (recommended for fast updates) or REST.

### MQTT (auto-discovery)
Enable the Mosquitto broker and the MQTT integration in Home Assistant, then set add-on options `mqtt_*`. The add-on publishes sensors via discovery under `homeassistant/sensor/*` and states under `home/internet_health/<link>/state`.

### REST (per link)
Example configuration.yaml (per link):

```yaml
sensor:
  - platform: rest
    name: Internet wan1 Status
    unique_id: internet_health_status_wan1
    resource: http://<addon-host>:8000/status/wan1
    method: GET
    value_template: "{{ value_json.status }}"
    json_attributes:
      - latency_ms
      - p95_ms
      - ok_ratio
      - window
      - last_checked
    timeout: 5
    scan_interval: 30

  - platform: rest
    name: Internet wan1 Score
    unique_id: internet_health_score_wan1
    resource: http://<addon-host>:8000/status/wan1
    method: GET
    value_template: "{{ value_json.score }}"
    timeout: 5
    scan_interval: 30

# Best link recommendation
rest:
  - resource: http://<addon-host>:8000/scoreboard
    sensor:
      - name: Internet Best Link
        value_template: "{{ value_json.recommended }}"
```

Optionally, create a template sensor with icon/colors based on state.

## Add-on Options
- `interval` (seconds) — probe interval.
- `timeout` (ms) — HTTP timeout per probe.
- `degraded_latency_ms` (ms) — default latency threshold.
- `window` — window size for ratios.
- `links_json` — e.g. `[{"name":"wan1","socks5":"socks5h://192.168.1.10:1080"},{"name":"wan2","socks5":"socks5h://192.168.2.10:1080"}]`
- `targets_json` — e.g. `["http://www.google.com/generate_204","http://cp.cloudflare.com/generate_204"]`

## Install as Home Assistant Add-on
- Use this repo as a custom add-on repository in Home Assistant (Settings → Add-ons → Add-on Store → ⋮ → Repositories → Add URL to your fork).
- Images are expected at `ghcr.io/<your-gh-user>/internet_health_checker-{arch}`. Edit `internet_health_checker/config.yaml:image` to your GHCR namespace and bump `version:` when releasing.
- The included GitHub Actions workflow builds and pushes per-arch images when you push a tag (e.g., `v0.1.0`).

Manual build (optional):
- `docker build -t internet-health -f internet_health_checker/Dockerfile --build-arg BUILD_FROM=ghcr.io/home-assistant/arm64-base-debian:trixie .`
- `docker run -p 8000:8000 internet-health`
