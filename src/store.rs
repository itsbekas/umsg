//! Fixed-size in-memory ring buffer for recent notifications.
//!
//! Zero heap allocation: each `Notification` stores its topic, title, and body
//! inline. Because `Store` is all-zero initialized, it lives in `.bss` (0 bytes
//! on disk, lazy-paged in RAM).

use crate::config::{MAX_BODY, MAX_HISTORY, MAX_TITLE, MAX_TOPIC};
use crate::fmt::BufWriter;

#[derive(Copy, Clone)]
pub struct Notification {
    pub id: u64,
    pub timestamp: i64,
    pub priority: u8,
    pub topic_len: u8,
    pub title_len: u8,
    pub body_len: u8,
    pub topic: [u8; MAX_TOPIC],
    pub title: [u8; MAX_TITLE],
    pub body: [u8; MAX_BODY],
}

impl Notification {
    pub const EMPTY: Self = Self {
        id: 0,
        timestamp: 0,
        priority: 0,
        topic_len: 0,
        title_len: 0,
        body_len: 0,
        topic: [0; MAX_TOPIC],
        title: [0; MAX_TITLE],
        body: [0; MAX_BODY],
    };

    pub fn topic(&self) -> &[u8] {
        &self.topic[..self.topic_len as usize]
    }

    pub fn title(&self) -> &[u8] {
        &self.title[..self.title_len as usize]
    }

    pub fn body(&self) -> &[u8] {
        &self.body[..self.body_len as usize]
    }

    /// Formats this notification as a single JSON object into `out` (no heap).
    /// Returns the number of bytes written.
    pub fn format_json(&self, out: &mut [u8]) -> usize {
        let mut w = BufWriter::new(out);
        w.push_bytes(b"{\"id\":");
        w.push_u64(self.id);
        w.push_bytes(b",\"ts\":");
        w.push_i64(self.timestamp);
        w.push_bytes(b",\"priority\":");
        w.push_u64(self.priority as u64);
        w.push_bytes(b",\"topic\":\"");
        w.push_json_escaped(self.topic());
        w.push_bytes(b"\",\"title\":\"");
        w.push_json_escaped(self.title());
        w.push_bytes(b"\",\"msg\":\"");
        w.push_json_escaped(self.body());
        w.push_bytes(b"\"}");
        w.len
    }
}

pub struct Store {
    entries: [Notification; MAX_HISTORY],
    next_id: u64,
    head: usize, // index where the next entry will be written
    count: usize,
}

/// Returns true if `name` is a safe topic identifier (`1..=MAX_TOPIC` of `[a-zA-Z0-9_-]`).
pub fn is_valid_topic(name: &[u8]) -> bool {
    if name.is_empty() || name.len() > MAX_TOPIC {
        return false;
    }
    for &b in name {
        if !(b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return false;
        }
    }
    true
}

/// Truncates `s` to at most `max` bytes without splitting a multi-byte UTF-8 sequence.
pub fn truncate_utf8(s: &[u8], max: usize) -> &[u8] {
    if s.len() <= max {
        return s;
    }
    let mut len = max;
    // UTF-8 continuation bytes have bit pattern 10xxxxxx (0x80..=0xBF)
    while len > 0 && (s[len] & 0b1100_0000) == 0b1000_0000 {
        len -= 1;
    }
    &s[..len]
}

impl Store {
    /// All-zero initializer so the entire store lives in `.bss`
    /// (0 bytes on disk, lazy-paged in RAM!).
    pub const EMPTY: Self = Self {
        entries: [Notification::EMPTY; MAX_HISTORY],
        next_id: 0,
        head: 0,
        count: 0,
    };

    pub fn push(
        &mut self,
        timestamp: i64,
        topic: &[u8],
        title: &[u8],
        priority: u8,
        body: &[u8],
    ) -> &Notification {
        let idx = self.head;
        let n = &mut self.entries[idx];

        self.next_id += 1;
        n.id = self.next_id;
        n.timestamp = timestamp;
        n.priority = priority;

        let topic = truncate_utf8(topic, MAX_TOPIC);
        n.topic[..topic.len()].copy_from_slice(topic);
        n.topic_len = topic.len() as u8;

        let title = truncate_utf8(title, MAX_TITLE);
        n.title[..title.len()].copy_from_slice(title);
        n.title_len = title.len() as u8;

        let body = truncate_utf8(body, MAX_BODY);
        n.body[..body.len()].copy_from_slice(body);
        n.body_len = body.len() as u8;

        self.head = (self.head + 1) % MAX_HISTORY;
        if self.count < MAX_HISTORY {
            self.count += 1;
        }

        &self.entries[idx]
    }

    /// Calls `f` for each stored notification, oldest to newest.
    pub fn for_each_oldest_first(&self, mut f: impl FnMut(&Notification)) {
        if self.count == 0 {
            return;
        }
        let start = if self.count < MAX_HISTORY {
            0
        } else {
            self.head
        };
        for i in 0..self.count {
            let idx = (start + i) % MAX_HISTORY;
            f(&self.entries[idx]);
        }
    }
}
