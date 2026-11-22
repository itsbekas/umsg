//! Default configuration & compile-time buffer capacities.
//!
//! Runtime deployment settings (`PORT`, `LOG_PATH`, `KEEPALIVE_SECS`) can be
//! overridden via environment variables (e.g. in `docker-compose.yml` / `.env`).
//! Fixed buffer sizes below are compile-time constants so all storage lives in
//! `.bss` with zero heap allocator.

/// Default TCP port (override at runtime with `PORT`).
pub const DEFAULT_PORT: u16 = 8080;

/// Default append-only binary log path (override at runtime with `LOG_PATH`).
pub const DEFAULT_LOG_PATH: &[u8] = b"notifications.log";

/// Default SSE keepalive interval in seconds (override at runtime with `KEEPALIVE_SECS`, clamped 1..=3600).
pub const DEFAULT_KEEPALIVE_SECS: i64 = 30;

/// Maximum seconds a non-SSE client may stay idle before being closed (Slowloris protection).
pub const READ_TIMEOUT_SECS: i64 = 10;

/// Maximum simultaneous in-flight HTTP requests (each holds a 4 KB read buffer).
pub const MAX_CLIENTS: usize = 16;

/// Maximum simultaneous live SSE subscribers (tiny ~40 B slots, no read buffer).
pub const MAX_SSE_CLIENTS: usize = 16;

/// Per-client HTTP read buffer size in bytes (4 KB handles Cloudflare Access headers).
pub const READ_BUF_SIZE: usize = 4096;

/// Temporary stack buffer size for formatting one JSON notification
/// (worst-case 2x escaped payload = 448 B + ~100 B JSON overhead < 640 B).
pub const JSON_BUF_SIZE: usize = 640;

/// Number of recent notifications kept in the in-memory ring buffer.
pub const MAX_HISTORY: usize = 64;

/// Maximum distinct interned topics in the topic table.
pub const MAX_TOPICS: usize = 16;

/// Maximum byte lengths for notification fields (all clean powers of 2).
pub const MAX_TOPIC: usize = 32;
pub const MAX_TITLE: usize = 64;
pub const MAX_BODY: usize = 128;
