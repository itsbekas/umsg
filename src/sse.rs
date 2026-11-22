//! Server-Sent Events (SSE) subscriber pool, framing, keepalive, and broadcast.

use rustix::event::epoll;
use rustix::fd::{AsFd, OwnedFd};

use crate::config::{JSON_BUF_SIZE, MAX_SSE_CLIENTS, MAX_TOPIC};
use crate::fmt::BufWriter;
use crate::http::send_all;
use crate::store::Notification;

pub struct SseClient {
    pub fd: Option<OwnedFd>,
    /// `0` means subscribed to `/all`; `> 0` means subscribed to `&topic[..topic_len]`.
    pub topic_len: u8,
    pub topic: [u8; MAX_TOPIC],
}

impl SseClient {
    pub const EMPTY: Self = Self {
        fd: None,
        topic_len: 0,
        topic: [0; MAX_TOPIC],
    };
}

pub fn close_sse_client(epfd: &OwnedFd, client: &mut SseClient) {
    if let Some(fd) = client.fd.take() {
        let _ = epoll::delete(epfd, &fd);
    }
    client.topic_len = 0;
}

pub fn send_sse_event(
    fd: impl AsFd,
    notif: &Notification,
    json_buf: &mut [u8; JSON_BUF_SIZE],
) -> bool {
    let len = notif.format_json(json_buf);
    send_sse_frame(fd, notif.id, &json_buf[..len])
}

pub fn send_sse_frame(fd: impl AsFd, id: u64, json: &[u8]) -> bool {
    let mut prefix = [0u8; 32];
    let mut w = BufWriter::new(&mut prefix);
    w.push_bytes(b"id: ");
    w.push_u64(id);
    w.push_bytes(b"\ndata: ");
    let prefix_len = w.len;

    send_all(&fd, &prefix[..prefix_len]) && send_all(&fd, json) && send_all(&fd, b"\n\n")
}

pub fn send_sse_keepalive(epfd: &OwnedFd, sse_clients: &mut [SseClient; MAX_SSE_CLIENTS]) {
    for client in sse_clients.iter_mut() {
        if let Some(fd) = client.fd.as_ref()
            && !send_all(fd, b": ping\n\n")
        {
            close_sse_client(epfd, client);
        }
    }
}

pub fn broadcast_sse_json(
    epfd: &OwnedFd,
    sse_clients: &mut [SseClient; MAX_SSE_CLIENTS],
    id: u64,
    topic: &[u8],
    topic_only: bool,
    json: &[u8],
) {
    for client in sse_clients.iter_mut() {
        let Some(fd) = client.fd.as_ref() else {
            continue;
        };
        let matches = if client.topic_len == 0 {
            !topic_only
        } else {
            &client.topic[..client.topic_len as usize] == topic
        };
        if matches && !send_sse_frame(fd, id, json) {
            close_sse_client(epfd, client);
        }
    }
}
