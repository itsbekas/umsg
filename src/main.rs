#![no_std]
#![no_main]

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
use rustix::time::{ClockId, clock_gettime};

use fmt::BufWriter;
use http::{Method, ParseStatus, parse_request};
use store::{Notification, Store};

const MAX_CLIENTS: usize = 16;
const READ_BUF_SIZE: usize = 1024;
const JSON_BUF_SIZE: usize = 512;

const LISTENER_TAG: u64 = u64::MAX;

struct Client {
    fd: Option<OwnedFd>,
    is_sse: bool,
    read_len: usize,
    read_buf: [u8; READ_BUF_SIZE],
}

impl Client {
    const EMPTY: Self = Self {
        fd: None,
        is_sse: false,
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

extern "C" fn entry() -> ! {
    // SAFETY: Single-threaded program; `entry` is called once and owns both statics exclusively.
    let clients = unsafe { &mut *core::ptr::addr_of_mut!(CLIENTS) };
    let store = unsafe { &mut *core::ptr::addr_of_mut!(STORE) };

    let listener = socket_with(
        AddressFamily::INET,
        SocketType::STREAM,
        SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
        None,
    )
    .unwrap_or_else(|_| exit(1));

    let _ = sockopt::set_socket_reuseaddr(&listener, true);

    let addr = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8080);
    if bind(&listener, &addr).is_err() || listen(&listener, 16).is_err() {
        exit(1);
    }

    let epfd = epoll::create(CreateFlags::CLOEXEC).unwrap_or_else(|_| exit(1));

    if epoll::add(&epfd, &listener, EventData::new_u64(LISTENER_TAG), EventFlags::IN).is_err() {
        exit(1);
    }

    // SAFETY: Single-threaded; FD 1 (stdout) is valid and never closed by us.
    let _ = io::write(
        unsafe { rustix::stdio::stdout() },
        b"Listening on http://127.0.0.1:8080\n",
    );

    let mut events = [MaybeUninit::<Event>::uninit(); MAX_CLIENTS];

    loop {
        let Ok((ready, _)) = epoll::wait(&epfd, &mut events, None) else {
            continue;
        };

        for ev in ready {
            let tag = ev.data.u64();
            let flags = ev.flags;

            if tag == LISTENER_TAG {
                accept_new_clients(&epfd, &listener, clients);
            } else {
                let slot = tag as usize;
                if slot < MAX_CLIENTS && clients[slot].fd.is_some() {
                    if flags.intersects(EventFlags::ERR | EventFlags::HUP | EventFlags::RDHUP) {
                        close_client(&epfd, &mut clients[slot]);
                    } else if flags.contains(EventFlags::IN) {
                        handle_client_read(&epfd, clients, store, slot);
                    }
                }
            }
        }
    }
}

fn accept_new_clients(epfd: &OwnedFd, listener: &OwnedFd, clients: &mut [Client; MAX_CLIENTS]) {
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
    client.read_len = 0;
}

fn handle_client_read(
    epfd: &OwnedFd,
    clients: &mut [Client; MAX_CLIENTS],
    store: &mut Store,
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

    let action = {
        let data = &clients[slot].read_buf[..clients[slot].read_len];
        match parse_request(data) {
            ParseStatus::Incomplete => return,
            ParseStatus::Invalid => 0u8, // close
            ParseStatus::Complete(req) => match (req.method, req.path) {
                (Method::Get, b"/events") => 1u8,
                (Method::Post, path) => {
                    let topic = path.strip_prefix(b"/").unwrap_or(path);
                    let topic = if topic.is_empty() { b"default" } else { topic };
                    let ts = clock_gettime(ClockId::Realtime).tv_sec;
                    let notif = *store.push(ts, topic, req.title, req.priority, req.body);
                    let topic_name = store.topic_name(notif.topic_id);
                    broadcast_id = notif.id;
                    broadcast_len = notif.format_json(topic_name, &mut json_buf);
                    2u8
                }
                _ => 3u8,
            },
        }
    };

    match action {
        1 => {
            // Upgrade connection to live SSE stream and replay recent history
            let sse_headers = b"HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream\r\n\
                Cache-Control: no-cache\r\n\
                Connection: keep-alive\r\n\r\n";

            let mut ok = clients[slot]
                .fd
                .as_ref()
                .is_some_and(|fd| send_all(fd, sse_headers));

            if ok {
                let fd = clients[slot].fd.as_ref().unwrap();
                store.for_each_oldest_first(|notif, topic_name| {
                    if ok {
                        ok = send_sse_event(fd, notif, topic_name, &mut json_buf);
                    }
                });
            }

            if ok {
                clients[slot].is_sse = true;
                clients[slot].read_len = 0;
            } else {
                close_client(epfd, &mut clients[slot]);
            }
        }
        2 => {
            // Reply to POST sender with the stored JSON notification
            if let Some(fd) = clients[slot].fd.as_ref() {
                send_http_response(fd, b"application/json", &json_buf[..broadcast_len]);
            }
            close_client(epfd, &mut clients[slot]);

            // Broadcast to all live SSE subscribers
            broadcast_sse_json(epfd, clients, broadcast_id, &json_buf[..broadcast_len]);
        }
        3 => {
            if let Some(fd) = clients[slot].fd.as_ref() {
                send_http_response(fd, b"text/plain", b"ok\n");
            }
            close_client(epfd, &mut clients[slot]);
        }
        _ => close_client(epfd, &mut clients[slot]),
    }
}

fn send_http_response(fd: impl AsFd, content_type: &[u8], body: &[u8]) {
    let mut hdr = [0u8; 160];
    let mut w = BufWriter::new(&mut hdr);
    w.push_bytes(b"HTTP/1.1 200 OK\r\nContent-Type: ");
    w.push_bytes(content_type);
    w.push_bytes(b"\r\nContent-Length: ");
    w.push_u64(body.len() as u64);
    w.push_bytes(b"\r\nConnection: close\r\n\r\n");
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

fn broadcast_sse_json(
    epfd: &OwnedFd,
    clients: &mut [Client; MAX_CLIENTS],
    id: u64,
    json: &[u8],
) {
    for client in clients.iter_mut() {
        if client.is_sse {
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
