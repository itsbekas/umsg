# μmsg (`umsg`)

An ultra-lightweight, `no_std` Linux HTTP messaging & pub-sub server written in Rust with zero `libc`, zero heap allocator, and zero threads.

- **Binary size:** `~22 KB`
- **Docker image (`FROM scratch`):** `~40 KB`
- **Live RAM (RSS):** `~48–64 KB` (`0.05 MB`)

## HTTP API

| Endpoint | Behavior |
|---|---|
| **`POST /<topic>`** | Publish a message to `<topic>` (`POST /` defaults to `default`). Persists to disk and broadcasts live. |
| **`GET /`** | Serves the embedded dark-mode web UI. |
| **`GET /all`** | Live SSE stream of **all** topics. Replays up to 64 recent messages (skipping `id <= Last-Event-ID`). |
| **`GET /<topic>`** | Live-only SSE stream for **`<topic>`** (no stale replay — safe for ephemeral messages). |
| **`GET /health`** | Returns `200 OK` with `ok\n`. |

### Examples

Publish a message:
```bash
curl -H "Title: Backup Complete" -H "Priority: high" \
  -d "Nightly backup finished in 42s." \
  http://localhost:8080/backups
```

Supported headers on `POST`:
- `Title` / `X-Title` — optional title (max `64` bytes)
- `Priority` / `X-Priority` — `1`/`min`, `2`/`low`, `3`/`default`, `4`/`high`, `5`/`urgent`/`max`
- `Ephemeral` / `X-Ephemeral` — `1`/`true`: broadcast live to `/<topic>` subscribers **only** (never persisted to RAM history, disk, or `/all`)
- `Authorization: Bearer <token>` — required on `POST` if `AUTH_TOKEN` is set
- Body — message text (max `128` bytes)

Subscribe live to a single topic:
```bash
curl -N http://localhost:8080/backups
```

## Configuration

**Runtime environment variables** (read directly from Linux's initial stack `envp` with 0 KB extra RAM):
- `PORT` — TCP port to bind on `0.0.0.0` (default: `8080`)
- `LOG_PATH` — path to the append-only binary log (default: `notifications.log`; `/data/notifications.log` in Docker)
- `KEEPALIVE_SECS` — SSE `: ping` interval in seconds, clamped `1..=3600` (default: `30`)
- `AUTH_TOKEN` — optional Bearer token required on `POST` requests (unset = no auth)

**Compile-time capacities** live in [src/config.rs](src/config.rs) (`MAX_CLIENTS`, `MAX_HISTORY`, `MAX_TOPICS`, `MAX_TOPIC`, `MAX_TITLE`, `MAX_BODY`).

## Build & Run

```bash
cargo build --release
./target/x86_64-unknown-linux-musl/release/umsg
```

Measure live RSS on Linux:
```bash
./measure.sh target/x86_64-unknown-linux-musl/release/umsg
```
