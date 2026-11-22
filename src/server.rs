//! HTTP connection lifecycle, request routing, and POST publishing.

use rustix::event::epoll::{self, EventData, EventFlags};
use rustix::fd::OwnedFd;
use rustix::io::{self, Errno};
use rustix::net::{SocketFlags, accept_with};
use rustix::time::{ClockId, clock_gettime};

use crate::config::{
    JSON_BUF_SIZE, MAX_BODY, MAX_CLIENTS, MAX_SSE_CLIENTS, MAX_TITLE, MAX_TOPIC, READ_BUF_SIZE,
};
use crate::disk::DiskLog;
use crate::http::{
    Method, ParseStatus, Request, const_time_eq, parse_request, send_all, send_http_status,
};
use crate::sse::{SseClient, broadcast_sse_json, send_sse_event};
use crate::store::{Notification, Store, is_valid_topic, truncate_utf8};

/// Embedded directly in `.rodata` — 1 memory page (4 KB), only paged in when requested.
const INDEX_HTML: &[u8] = include_bytes!("index.html");

pub struct HttpClient {
    pub fd: Option<OwnedFd>,
    pub connected_at: i64, // Monotonic seconds for total HTTP request deadline
    pub read_len: usize,
    pub read_buf: [u8; READ_BUF_SIZE],
}

impl HttpClient {
    pub const EMPTY: Self = Self {
        fd: None,
        connected_at: 0,
        read_len: 0,
        read_buf: [0; READ_BUF_SIZE],
    };
}

enum Action {
    UpgradeSse,
    PostOk,
    IndexHtml,
    Health,
    BadRequest,
    NotFound,
    TopicLimit,
    Unauthorized,
}

pub fn accept_new_clients(
    epfd: &OwnedFd,
    listener: &OwnedFd,
    http_clients: &mut [HttpClient; MAX_CLIENTS],
    now: i64,
) {
    loop {
        let Ok(fd) = accept_with(listener, SocketFlags::NONBLOCK | SocketFlags::CLOEXEC) else {
            break;
        };

        for (slot, client) in http_clients.iter_mut().enumerate() {
            if client.fd.is_none() {
                if epoll::add(
                    epfd,
                    &fd,
                    EventData::new_u64(slot as u64),
                    EventFlags::IN | EventFlags::RDHUP,
                )
                .is_ok()
                {
                    client.fd = Some(fd);
                    client.connected_at = now;
                    client.read_len = 0;
                }
                break;
            }
        }
    }
}

pub fn close_http_client(epfd: &OwnedFd, client: &mut HttpClient) {
    if let Some(fd) = client.fd.take() {
        let _ = epoll::delete(epfd, &fd);
    }
    client.read_len = 0;
}

pub fn handle_client_read(
    epfd: &OwnedFd,
    http_clients: &mut [HttpClient; MAX_CLIENTS],
    sse_clients: &mut [SseClient; MAX_SSE_CLIENTS],
    store: &mut Store,
    disk: &DiskLog,
    auth_token: Option<&[u8]>,
    slot: usize,
) {
    let len = http_clients[slot].read_len;
    if len >= READ_BUF_SIZE {
        close_http_client(epfd, &mut http_clients[slot]);
        return;
    }

    let read_res = {
        let client = &mut http_clients[slot];
        let Some(fd) = client.fd.as_ref() else { return };
        io::read(fd, &mut client.read_buf[len..])
    };

    let n = match read_res {
        Ok(0) => {
            close_http_client(epfd, &mut http_clients[slot]);
            return;
        }
        Ok(n) => n,
        Err(Errno::AGAIN) => return,
        Err(_) => {
            close_http_client(epfd, &mut http_clients[slot]);
            return;
        }
    };

    http_clients[slot].read_len += n;

    // Parse the HTTP request in-place from read_buf (zero allocations!)
    let mut json_buf = [0u8; JSON_BUF_SIZE];
    let mut broadcast_len: usize = 0;
    let mut broadcast_id: u64 = 0;
    let mut topic_buf = [0u8; MAX_TOPIC];
    let mut topic_len: usize = 0;
    let mut broadcast_topic_only = false;
    let mut replay_history = false;
    let mut last_event_id: u64 = 0;

    let action = {
        let data = &http_clients[slot].read_buf[..http_clients[slot].read_len];
        match parse_request(data) {
            ParseStatus::Incomplete => return,
            ParseStatus::Invalid => Action::BadRequest,
            ParseStatus::Complete(req) => {
                if matches!((req.method, req.path), (Method::Get, b"/health")) {
                    Action::Health
                } else if let Some(expected) = auth_token {
                    let supplied = req.auth.strip_prefix(b"Bearer ").unwrap_or(b"");
                    if !const_time_eq(supplied, expected) {
                        Action::Unauthorized
                    } else {
                        route_authorized(
                            store,
                            disk,
                            &req,
                            &mut json_buf,
                            &mut broadcast_id,
                            &mut topic_buf,
                            &mut topic_len,
                            &mut broadcast_len,
                            &mut broadcast_topic_only,
                            &mut replay_history,
                            &mut last_event_id,
                        )
                    }
                } else {
                    route_authorized(
                        store,
                        disk,
                        &req,
                        &mut json_buf,
                        &mut broadcast_id,
                        &mut topic_buf,
                        &mut topic_len,
                        &mut broadcast_len,
                        &mut broadcast_topic_only,
                        &mut replay_history,
                        &mut last_event_id,
                    )
                }
            }
        }
    };

    match action {
        Action::UpgradeSse => {
            let Some(sse_slot) = sse_clients.iter().position(|c| c.fd.is_none()) else {
                if let Some(fd) = http_clients[slot].fd.as_ref() {
                    send_http_status(
                        fd,
                        b"503 Service Unavailable",
                        b"text/plain",
                        b"too many subscribers\n",
                    );
                }
                close_http_client(epfd, &mut http_clients[slot]);
                return;
            };

            let sse_headers = b"HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream\r\n\
                Cache-Control: no-cache\r\n\
                X-Content-Type-Options: nosniff\r\n\
                Connection: keep-alive\r\n\r\n";

            let mut ok = http_clients[slot]
                .fd
                .as_ref()
                .is_some_and(|fd| send_all(fd, sse_headers));

            if ok && replay_history {
                let fd = http_clients[slot].fd.as_ref().unwrap();
                store.for_each_oldest_first(|notif, topic_name| {
                    if ok && notif.id > last_event_id {
                        ok = send_sse_event(fd, notif, topic_name, &mut json_buf);
                    }
                });
            }

            if ok {
                // Move FD out of the 4 KB HTTP slot into the tiny ~40 B SSE slot
                // and watch RDHUP only (no EPOLLIN) so HTTP slots stay free for POSTs.
                if let Some(fd) = http_clients[slot].fd.take() {
                    http_clients[slot].read_len = 0;
                    if epoll::modify(
                        epfd,
                        &fd,
                        EventData::new_u64((MAX_CLIENTS + sse_slot) as u64),
                        EventFlags::RDHUP,
                    )
                    .is_ok()
                    {
                        let sse = &mut sse_clients[sse_slot];
                        sse.fd = Some(fd);
                        sse.topic_len = topic_len as u8;
                        if topic_len > 0 {
                            sse.topic[..topic_len].copy_from_slice(&topic_buf[..topic_len]);
                        }
                    } else {
                        let _ = epoll::delete(epfd, &fd);
                    }
                }
            } else {
                close_http_client(epfd, &mut http_clients[slot]);
            }
        }
        Action::PostOk => {
            if let Some(fd) = http_clients[slot].fd.as_ref() {
                send_http_status(
                    fd,
                    b"200 OK",
                    b"application/json",
                    &json_buf[..broadcast_len],
                );
            }
            close_http_client(epfd, &mut http_clients[slot]);

            broadcast_sse_json(
                epfd,
                sse_clients,
                broadcast_id,
                &topic_buf[..topic_len],
                broadcast_topic_only,
                &json_buf[..broadcast_len],
            );
        }
        Action::IndexHtml => {
            if let Some(fd) = http_clients[slot].fd.as_ref() {
                send_http_status(fd, b"200 OK", b"text/html; charset=utf-8", INDEX_HTML);
            }
            close_http_client(epfd, &mut http_clients[slot]);
        }
        Action::Health => {
            if let Some(fd) = http_clients[slot].fd.as_ref() {
                send_http_status(fd, b"200 OK", b"text/plain", b"ok\n");
            }
            close_http_client(epfd, &mut http_clients[slot]);
        }
        Action::BadRequest => {
            if let Some(fd) = http_clients[slot].fd.as_ref() {
                send_http_status(fd, b"400 Bad Request", b"text/plain", b"bad request\n");
            }
            close_http_client(epfd, &mut http_clients[slot]);
        }
        Action::NotFound => {
            if let Some(fd) = http_clients[slot].fd.as_ref() {
                send_http_status(fd, b"404 Not Found", b"text/plain", b"not found\n");
            }
            close_http_client(epfd, &mut http_clients[slot]);
        }
        Action::TopicLimit => {
            if let Some(fd) = http_clients[slot].fd.as_ref() {
                send_http_status(
                    fd,
                    b"507 Insufficient Storage",
                    b"text/plain",
                    b"topic limit reached\n",
                );
            }
            close_http_client(epfd, &mut http_clients[slot]);
        }
        Action::Unauthorized => {
            if let Some(fd) = http_clients[slot].fd.as_ref() {
                send_http_status(fd, b"401 Unauthorized", b"text/plain", b"unauthorized\n");
            }
            close_http_client(epfd, &mut http_clients[slot]);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn route_authorized(
    store: &mut Store,
    disk: &DiskLog,
    req: &Request<'_>,
    json_buf: &mut [u8; JSON_BUF_SIZE],
    broadcast_id: &mut u64,
    topic_buf: &mut [u8; MAX_TOPIC],
    topic_len: &mut usize,
    broadcast_len: &mut usize,
    broadcast_topic_only: &mut bool,
    replay_history: &mut bool,
    last_event_id: &mut u64,
) -> Action {
    match (req.method, req.path) {
        (Method::Get, b"/") => Action::IndexHtml,
        (Method::Get, b"/all") => {
            *last_event_id = req.last_event_id;
            *topic_len = 0;
            *replay_history = true;
            Action::UpgradeSse
        }
        (Method::Get, path) => {
            let topic = path.strip_prefix(b"/").unwrap_or(path);
            if !is_valid_topic(topic) {
                Action::NotFound
            } else {
                topic_buf[..topic.len()].copy_from_slice(topic);
                *topic_len = topic.len();
                *replay_history = false; // live-only (safe for ephemeral tokens)
                Action::UpgradeSse
            }
        }
        (Method::Post, path) => handle_post(
            store,
            disk,
            path,
            req,
            json_buf,
            broadcast_id,
            topic_buf,
            topic_len,
            broadcast_len,
            broadcast_topic_only,
        ),
        _ => Action::NotFound,
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_post(
    store: &mut Store,
    disk: &DiskLog,
    path: &[u8],
    req: &Request<'_>,
    json_buf: &mut [u8; JSON_BUF_SIZE],
    broadcast_id: &mut u64,
    topic_buf: &mut [u8; MAX_TOPIC],
    topic_len: &mut usize,
    broadcast_len: &mut usize,
    broadcast_topic_only: &mut bool,
) -> Action {
    let topic = path.strip_prefix(b"/").unwrap_or(path);
    let topic = if topic.is_empty() { b"default" } else { topic };
    if !is_valid_topic(topic) {
        return Action::BadRequest;
    }

    topic_buf[..topic.len()].copy_from_slice(topic);
    *topic_len = topic.len();

    let ts = clock_gettime(ClockId::Realtime).tv_sec;

    if req.ephemeral {
        // Ephemeral message (e.g. 2FA code): broadcast live to /<topic> ONLY,
        // never stored in ring buffer, never written to disk, never interns a topic slot,
        // and never sent to /all.
        let mut tmp = Notification::EMPTY;
        tmp.id = 0;
        tmp.timestamp = ts;
        tmp.priority = req.priority;
        let title = truncate_utf8(req.title, MAX_TITLE);
        tmp.title[..title.len()].copy_from_slice(title);
        tmp.title_len = title.len() as u8;
        let body = truncate_utf8(req.body, MAX_BODY);
        tmp.body[..body.len()].copy_from_slice(body);
        tmp.body_len = body.len() as u8;

        *broadcast_id = 0;
        *broadcast_topic_only = true;
        *broadcast_len = tmp.format_json(topic, json_buf);
        return Action::PostOk;
    }

    if let Some(notif) = store
        .push(ts, topic, req.title, req.priority, req.body)
        .copied()
    {
        let topic_name = store.topic_name(notif.topic_id);
        disk.append(&notif, topic_name);
        *broadcast_id = notif.id;
        *broadcast_topic_only = false;
        *broadcast_len = notif.format_json(topic_name, json_buf);
        Action::PostOk
    } else {
        Action::TopicLimit
    }
}
