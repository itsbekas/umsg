//! Append-only binary log for persisting notifications across restarts.
//!
//! Record wire layout (length-prefixed, little-endian):
//!   [timestamp: i64 (8B)]
//!   [priority:  u8  (1B)]
//!   [topic_len: u8  (1B)]
//!   [title_len: u8  (1B)]
//!   [body_len:  u8  (1B)]
//!   [topic bytes]
//!   [title bytes]
//!   [body bytes]
//!
//! Max record size = 12 + 32 + 64 + 128 = 236 bytes (fits in a 256-byte stack buffer!).
//! Uses ~0 KB of persistent RAM — only temporary stack buffers during read/write.

use core::ffi::CStr;

use rustix::fd::OwnedFd;
use rustix::fs::{CWD, Mode, OFlags, openat};
use rustix::io::{self, Errno};

use crate::store::{MAX_BODY, MAX_TITLE, MAX_TOPIC, Notification, Store};

const HEADER_LEN: usize = 12;
const MAX_RECORD_LEN: usize = HEADER_LEN + MAX_TOPIC + MAX_TITLE + MAX_BODY; // 236 bytes

pub struct DiskLog {
    fd: Option<OwnedFd>,
}

impl DiskLog {
    /// Opens (or creates) `path` for reading & appending, and replays any
    /// existing records into `store`.
    pub fn open_and_load(path: &CStr, store: &mut Store) -> Self {
        let flags = OFlags::RDWR | OFlags::CREATE | OFlags::APPEND | OFlags::CLOEXEC;
        let mode = Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH; // 0644

        let Ok(fd) = openat(CWD, path, flags, mode) else {
            return Self { fd: None };
        };

        // Replay existing records from the start of the file into `store`
        // (O_APPEND only affects writes; reads start at offset 0).
        let mut buf = [0u8; 4096];
        let mut filled: usize = 0;

        loop {
            match io::read(&fd, &mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => {
                    filled += n;
                    let mut pos = 0;

                    while filled - pos >= HEADER_LEN {
                        let ts = i64::from_le_bytes([
                            buf[pos],
                            buf[pos + 1],
                            buf[pos + 2],
                            buf[pos + 3],
                            buf[pos + 4],
                            buf[pos + 5],
                            buf[pos + 6],
                            buf[pos + 7],
                        ]);
                        let priority = buf[pos + 8];
                        let tlen = buf[pos + 9] as usize;
                        let ttlen = buf[pos + 10] as usize;
                        let blen = buf[pos + 11] as usize;

                        // Sanity check against corrupted records
                        if tlen > MAX_TOPIC || ttlen > MAX_TITLE || blen > MAX_BODY {
                            filled = 0;
                            pos = 0;
                            break;
                        }

                        let rec_len = HEADER_LEN + tlen + ttlen + blen;
                        if filled - pos < rec_len {
                            break; // wait for more bytes in next read
                        }

                        let topic_start = pos + HEADER_LEN;
                        let title_start = topic_start + tlen;
                        let body_start = title_start + ttlen;

                        store.push(
                            ts,
                            &buf[topic_start..title_start],
                            &buf[title_start..body_start],
                            priority,
                            &buf[body_start..body_start + blen],
                        );

                        pos += rec_len;
                    }

                    // Shift any leftover partial record to the front of `buf`
                    let rem = filled - pos;
                    if rem > 0 && pos > 0 {
                        buf.copy_within(pos..filled, 0);
                    }
                    filled = rem;
                }
                Err(Errno::INTR) => continue,
                Err(_) => break,
            }
        }

        Self { fd: Some(fd) }
    }

    /// Appends one notification record to disk using a single stack buffer & write.
    pub fn append(&self, notif: &Notification, topic: &[u8]) {
        let Some(fd) = self.fd.as_ref() else { return };

        let tlen = topic.len().min(MAX_TOPIC);
        let title = notif.title();
        let body = notif.body();

        let mut rec = [0u8; MAX_RECORD_LEN];
        rec[0..8].copy_from_slice(&notif.timestamp.to_le_bytes());
        rec[8] = notif.priority;
        rec[9] = tlen as u8;
        rec[10] = title.len() as u8;
        rec[11] = body.len() as u8;

        let mut pos = HEADER_LEN;
        rec[pos..pos + tlen].copy_from_slice(&topic[..tlen]);
        pos += tlen;
        rec[pos..pos + title.len()].copy_from_slice(title);
        pos += title.len();
        rec[pos..pos + body.len()].copy_from_slice(body);
        pos += body.len();

        let mut written = 0;
        while written < pos {
            match io::write(fd, &rec[written..pos]) {
                Ok(0) => break,
                Ok(n) => written += n,
                Err(Errno::INTR) => continue,
                Err(_) => break,
            }
        }
    }
}
