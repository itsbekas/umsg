#![no_std]
#![no_main]

mod config;
mod disk;
mod env;
mod fmt;
mod http;
mod mem;
mod store;

use core::mem::MaybeUninit;
use core::net::Ipv4Addr;
use core::panic::PanicInfo;

use rustix::event::epoll::{self, CreateFlags, Event, EventData, EventFlags};
use rustix::fd::{AsFd, OwnedFd};
use rustix::io::{self, Errno};
use rustix::net::{
    AddressFamily, SendFlags, SocketAddrV4, SocketFlags, SocketType, accept_with, bind, listen,
    send, socket_with, sockopt,
};
use rustix::time::{ClockId, Timespec, clock_gettime};

use config::{
    DEFAULT_KEEPALIVE_SECS, DEFAULT_LOG_PATH, DEFAULT_PORT, JSON_BUF_SIZE, MAX_BODY, MAX_CLIENTS,
    MAX_TITLE, MAX_TOPIC, READ_BUF_SIZE, READ_TIMEOUT_SECS,
};
use disk::DiskLog;
use env::Env;
use fmt::BufWriter;
use http::{Method, ParseStatus, parse_request};
use store::{Notification, Store, is_valid_topic, truncate_utf8};

/// Embedded directly in `.rodata` — 1 memory page (4 KB), only paged in when requested.
const INDEX_HTML: &[u8] = include_bytes!("index.html");

const LISTENER_TAG: u64 = u64::MAX;

struct Client {
    fd: Option<OwnedFd>,
    is_sse: bool,
    /// When `is_sse == true`: `0` means subscribed to `/all`; `> 0` means subscribed
    /// to `&read_buf[..sub_topic_len]` (reusing the idle read buffer for 0 extra RAM!).
    sub_topic_len: u8,
    connected_at: i64, // Monotonic seconds for total non-SSE request deadline
    read_len: usize,
    read_buf: [u8; READ_BUF_SIZE],
}

impl Client {
    const EMPTY: Self = Self {
        fd: None,
        is_sse: false,
        sub_topic_len: 0,
        connected_at: 0,
        read_len: 0,
        read_buf: [0; READ_BUF_SIZE],
    };
}

/// Static zero-initialized storage (.bss).
/// Linux lazy-pages .bss so unused client slots and unused history entries
/// consume 0 KB of physical RAM until written.
static mut CLIENTS: [Client; MAX_CLIENTS] = [const { Client::EMPTY }; MAX_CLIENTS];
static mut STORE: Store = Store::EMPTY;

#[unsafe(no_mangle)]
#[unsafe(naked)]
pub unsafe extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "mov rdi, rsp",
        "and rsp, -16",
        "call {entry}",
        entry = sym entry,
    );
}

fn exit(code: i32) -> ! {
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") 231usize, // SYS_exit_group
            in("rdi") code as usize,
            options(noreturn, nostack),
        );
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    exit(1)
}

extern "C" fn entry(initial_sp: *const usize) -> ! {
    // SAFETY: `initial_sp` is the exact `rsp` handed to `_start` by Linux.
    let env = unsafe { Env::from_initial_sp(initial_sp) };
    let port = env
        .get_u64(b"PORT")
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(DEFAULT_PORT);
    let log_path = env.get(b"LOG_PATH").unwrap_or(DEFAULT_LOG_PATH);
    let keepalive_secs = env
        .get_u64(b"KEEPALIVE_SECS")
        .map(|s| s.clamp(1, 3600) as i64)
        .unwrap_or(DEFAULT_KEEPALIVE_SECS);
    let auth_token = env.get(b"AUTH_TOKEN").filter(|t| !t.is_empty());

    // SAFETY: Single-threaded program; `entry` is called once and owns both statics exclusively.
    let clients = unsafe { &mut *core::ptr::addr_of_mut!(CLIENTS) };
    let store = unsafe { &mut *core::ptr::addr_of_mut!(STORE) };
    let disk = DiskLog::open_and_load(log_path, store);

    let listener = socket_with(
        AddressFamily::INET,
        SocketType::STREAM,
        SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
        None,
    )
    .unwrap_or_else(|_| exit(1));

    let _ = sockopt::set_socket_reuseaddr(&listener, true);

    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
    if bind(&listener, &addr).is_err() || listen(&listener, 16).is_err() {
        exit(1);
    }

    let epfd = epoll::create(CreateFlags::CLOEXEC).unwrap_or_else(|_| exit(1));

    if epoll::add(&epfd, &listener, EventData::new_u64(LISTENER_TAG), EventFlags::IN).is_err() {
        exit(1);
    }

    let mut banner = [0u8; 64];
    let mut bw = BufWriter::new(&mut banner);
    bw.push_bytes(b"Listening on http://0.0.0.0:");
    bw.push_u64(port as u64);
    bw.push_byte(b'\n');
    let banner_len = bw.len;
    // SAFETY: Single-threaded; FD 1 (stdout) is valid and never closed by us.
    let _ = io::write(unsafe { rustix::stdio::stdout() }, &banner[..banner_len]);

    let mut events = [MaybeUninit::<Event>::uninit(); MAX_CLIENTS];
    let poll_tick = Timespec {
        tv_sec: keepalive_secs.min(5),
        tv_nsec: 0,
    };
    let mut last_keepalive = monotonic_secs();

    loop {
        let ready_count = match epoll::wait(&epfd, &mut events, Some(&poll_tick)) {
            Ok((ready, _)) => ready.len(),
            Err(_) => 0,
        };

        let now = monotonic_secs();

        for ev in &events[..ready_count] {
            // SAFETY: kernel initialized `events[..ready_count]`.
            let ev = unsafe { ev.assume_init_ref() };
            let tag = ev.data.u64();
            let flags = ev.flags;

            if tag == LISTENER_TAG {
                accept_new_clients(&epfd, &listener, clients, now);
            } else {
                let slot = tag as usize;
                if slot < MAX_CLIENTS && clients[slot].fd.is_some() {
                    if flags.intersects(EventFlags::ERR | EventFlags::HUP | EventFlags::RDHUP) {
                        close_client(&epfd, &mut clients[slot]);
                    } else if flags.contains(EventFlags::IN) {
                        handle_client_read(&epfd, clients, store, &disk, auth_token, slot);
                    }
                }
            }
        }

        // Reap slow/idle non-SSE connections (hard deadline defeats trickle Slowloris)
        for client in clients.iter_mut() {
            if client.fd.is_some()
                && !client.is_sse
                && now.saturating_sub(client.connected_at) >= READ_TIMEOUT_SECS
            {
                close_client(&epfd, client);
            }
        }

        // Monotonic SSE keepalive (cannot be starved by activity on other sockets)
        if now.saturating_sub(last_keepalive) >= keepalive_secs {
            send_sse_keepalive(&epfd, clients);
            last_keepalive = now;
        }
    }
}

fn monotonic_secs() -> i64 {
    clock_gettime(ClockId::Monotonic).tv_sec
}

fn accept_new_clients(
    epfd: &OwnedFd,
    listener: &OwnedFd,
    clients: &mut [Client; MAX_CLIENTS],
    now: i64,
) {
    loop {
        let Ok(fd) = accept_with(listener, SocketFlags::NONBLOCK | SocketFlags::CLOEXEC) else {
            break;
        };

        for (slot, client) in clients.iter_mut().enumerate() {
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
                    client.is_sse = false;
                    client.sub_topic_len = 0;
                    client.connected_at = now;
                    client.read_len = 0;
                }
                break;
            }
        }
    }
}

fn close_client(epfd: &OwnedFd, client: &mut Client) {
    if let Some(fd) = client.fd.take() {
        let _ = epoll::delete(epfd, &fd);
    }
    client.is_sse = false;
    client.sub_topic_len = 0;
    client.read_len = 0;
}

fn handle_client_read(
    epfd: &OwnedFd,
    clients: &mut [Client; MAX_CLIENTS],
    store: &mut Store,
    disk: &DiskLog,
    auth_token: Option<&[u8]>,
    slot: usize,
) {
    if clients[slot].is_sse {
        let mut tmp = [0u8; 64];
        let closed = match clients[slot].fd.as_ref().map(|fd| io::read(fd, &mut tmp)) {
            Some(Ok(0)) => true,
            Some(Err(e)) if e != Errno::AGAIN => true,
            _ => false,
        };
        if closed {
            close_client(epfd, &mut clients[slot]);
        }
        return;
    }

    let len = clients[slot].read_len;
    if len >= READ_BUF_SIZE {
        close_client(epfd, &mut clients[slot]);
        return;
    }

    let read_res = {
        let client = &mut clients[slot];
        let Some(fd) = client.fd.as_ref() else { return };
        io::read(fd, &mut client.read_buf[len..])
    };

    let n = match read_res {
        Ok(0) => {
            close_client(epfd, &mut clients[slot]);
            return;
        }
        Ok(n) => n,
        Err(Errno::AGAIN) => return,
        Err(_) => {
            close_client(epfd, &mut clients[slot]);
            return;
        }
    };

    clients[slot].read_len += n;

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
        let data = &clients[slot].read_buf[..clients[slot].read_len];
        match parse_request(data) {
            ParseStatus::Incomplete => return,
            ParseStatus::Invalid => 5u8, // 400 Bad Request
            ParseStatus::Complete(req) => match (req.method, req.path) {
                (Method::Get, b"/") => 3u8,
                (Method::Get, b"/health") => 4u8,
                (Method::Get, b"/all") => {
                    last_event_id = req.last_event_id;
                    topic_len = 0;
                    replay_history = true;
                    1u8
                }
                (Method::Get, path) => {
                    let topic = path.strip_prefix(b"/").unwrap_or(path);
                    if !is_valid_topic(topic) {
                        6u8 // 404 Not Found (e.g. /favicon.ico, invalid chars)
                    } else {
                        // Store topic name without touching Store's topic table!
                        topic_buf[..topic.len()].copy_from_slice(topic);
                        topic_len = topic.len();
                        replay_history = false; // live-only (safe for ephemeral tokens)
                        1u8
                    }
                }
                (Method::Post, path) => {
                    if let Some(expected) = auth_token {
                        let supplied = req.auth.strip_prefix(b"Bearer ").unwrap_or(b"");
                        if !const_time_eq(supplied, expected) {
                            8u8 // 401 Unauthorized
                        } else {
                            handle_post(
                                store,
                                disk,
                                path,
                                &req,
                                &mut json_buf,
                                &mut broadcast_id,
                                &mut topic_buf,
                                &mut topic_len,
                                &mut broadcast_len,
                                &mut broadcast_topic_only,
                            )
                        }
                    } else {
                        handle_post(
                            store,
                            disk,
                            path,
                            &req,
                            &mut json_buf,
                            &mut broadcast_id,
                            &mut topic_buf,
                            &mut topic_len,
                            &mut broadcast_len,
                            &mut broadcast_topic_only,
                        )
                    }
                }
                _ => 6u8, // 404 Not Found
            },
        }
    };

    match action {
        1 => {
            // Upgrade connection to live SSE stream
            let sse_headers = b"HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream\r\n\
                Cache-Control: no-cache\r\n\
                X-Content-Type-Options: nosniff\r\n\
                Connection: keep-alive\r\n\r\n";

            let mut ok = clients[slot]
                .fd
                .as_ref()
                .is_some_and(|fd| send_all(fd, sse_headers));

            if ok && replay_history {
                let fd = clients[slot].fd.as_ref().unwrap();
                store.for_each_oldest_first(|notif, topic_name| {
                    if ok && notif.id > last_event_id {
                        ok = send_sse_event(fd, notif, topic_name, &mut json_buf);
                    }
                });
            }

            if ok {
                clients[slot].is_sse = true;
                clients[slot].sub_topic_len = topic_len as u8;
                if topic_len > 0 {
                    clients[slot].read_buf[..topic_len].copy_from_slice(&topic_buf[..topic_len]);
                }
                clients[slot].read_len = 0;
            } else {
                close_client(epfd, &mut clients[slot]);
            }
        }
        2 => {
            // Reply to POST sender with the stored JSON notification
            if let Some(fd) = clients[slot].fd.as_ref() {
                send_http_status(
                    fd,
                    b"200 OK",
                    b"application/json",
                    &json_buf[..broadcast_len],
                );
            }
            close_client(epfd, &mut clients[slot]);

            // Broadcast to subscribers (ephemeral messages go ONLY to matching /<topic> subscribers)
            broadcast_sse_json(
                epfd,
                clients,
                broadcast_id,
                &topic_buf[..topic_len],
                broadcast_topic_only,
                &json_buf[..broadcast_len],
            );
        }
        3 => {
            // Serve the embedded web UI
            if let Some(fd) = clients[slot].fd.as_ref() {
                send_http_status(fd, b"200 OK", b"text/html; charset=utf-8", INDEX_HTML);
            }
            close_client(epfd, &mut clients[slot]);
        }
        4 => {
            if let Some(fd) = clients[slot].fd.as_ref() {
                send_http_status(fd, b"200 OK", b"text/plain", b"ok\n");
            }
            close_client(epfd, &mut clients[slot]);
        }
        5 => {
            if let Some(fd) = clients[slot].fd.as_ref() {
                send_http_status(fd, b"400 Bad Request", b"text/plain", b"bad request\n");
            }
            close_client(epfd, &mut clients[slot]);
        }
        6 => {
            if let Some(fd) = clients[slot].fd.as_ref() {
                send_http_status(fd, b"404 Not Found", b"text/plain", b"not found\n");
            }
            close_client(epfd, &mut clients[slot]);
        }
        7 => {
            if let Some(fd) = clients[slot].fd.as_ref() {
                send_http_status(
                    fd,
                    b"507 Insufficient Storage",
                    b"text/plain",
                    b"topic limit reached\n",
                );
            }
            close_client(epfd, &mut clients[slot]);
        }
        8 => {
            if let Some(fd) = clients[slot].fd.as_ref() {
                send_http_status(fd, b"401 Unauthorized", b"text/plain", b"unauthorized\n");
            }
            close_client(epfd, &mut clients[slot]);
        }
        _ => close_client(epfd, &mut clients[slot]),
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_post(
    store: &mut Store,
    disk: &DiskLog,
    path: &[u8],
    req: &http::Request<'_>,
    json_buf: &mut [u8; JSON_BUF_SIZE],
    broadcast_id: &mut u64,
    topic_buf: &mut [u8; MAX_TOPIC],
    topic_len: &mut usize,
    broadcast_len: &mut usize,
    broadcast_topic_only: &mut bool,
) -> u8 {
    let topic = path.strip_prefix(b"/").unwrap_or(path);
    let topic = if topic.is_empty() { b"default" } else { topic };
    if !is_valid_topic(topic) {
        return 5; // 400 Bad Request
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
        return 2;
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
        2
    } else {
        7 // 507 Insufficient Storage
    }
}

/// Constant-time byte slice comparison (does not short-circuit on mismatch).
fn const_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn send_http_status(fd: impl AsFd, status: &[u8], content_type: &[u8], body: &[u8]) {
    let mut hdr = [0u8; 384];
    let mut w = BufWriter::new(&mut hdr);
    w.push_bytes(b"HTTP/1.1 ");
    w.push_bytes(status);
    w.push_bytes(b"\r\nContent-Type: ");
    w.push_bytes(content_type);
    w.push_bytes(b"\r\nContent-Length: ");
    w.push_u64(body.len() as u64);
    w.push_bytes(
        b"\r\nX-Content-Type-Options: nosniff\r\n\
        X-Frame-Options: DENY\r\n\
        Referrer-Policy: no-referrer\r\n\
        Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; img-src data:\r\n\
        Connection: close\r\n\r\n",
    );
    let hdr_len = w.len;

    if send_all(&fd, &hdr[..hdr_len]) {
        let _ = send_all(&fd, body);
    }
}

fn send_sse_event(
    fd: impl AsFd,
    notif: &Notification,
    topic_name: &[u8],
    json_buf: &mut [u8; JSON_BUF_SIZE],
) -> bool {
    let len = notif.format_json(topic_name, json_buf);
    send_sse_frame(fd, notif.id, &json_buf[..len])
}

fn send_sse_frame(fd: impl AsFd, id: u64, json: &[u8]) -> bool {
    let mut prefix = [0u8; 32];
    let mut w = BufWriter::new(&mut prefix);
    w.push_bytes(b"id: ");
    w.push_u64(id);
    w.push_bytes(b"\ndata: ");
    let prefix_len = w.len;

    send_all(&fd, &prefix[..prefix_len]) && send_all(&fd, json) && send_all(&fd, b"\n\n")
}

fn send_sse_keepalive(epfd: &OwnedFd, clients: &mut [Client; MAX_CLIENTS]) {
    for client in clients.iter_mut() {
        if client.is_sse {
            let ok = client
                .fd
                .as_ref()
                .is_some_and(|fd| send_all(fd, b": ping\n\n"));
            if !ok {
                close_client(epfd, client);
            }
        }
    }
}

fn broadcast_sse_json(
    epfd: &OwnedFd,
    clients: &mut [Client; MAX_CLIENTS],
    id: u64,
    topic: &[u8],
    topic_only: bool,
    json: &[u8],
) {
    for client in clients.iter_mut() {
        let matches = if client.sub_topic_len == 0 {
            !topic_only
        } else {
            &client.read_buf[..client.sub_topic_len as usize] == topic
        };
        if client.is_sse && matches {
            let ok = client
                .fd
                .as_ref()
                .is_some_and(|fd| send_sse_frame(fd, id, json));
            if !ok {
                close_client(epfd, client);
            }
        }
    }
}

fn send_all(fd: impl AsFd, mut buf: &[u8]) -> bool {
    while !buf.is_empty() {
        match send(&fd, buf, SendFlags::NOSIGNAL) {
            Ok(0) => return false,
            Ok(n) => buf = &buf[n..],
            Err(Errno::INTR) => continue,
            Err(_) => return false,
        }
    }
    true
}
