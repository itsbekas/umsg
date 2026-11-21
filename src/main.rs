#![no_std]
#![no_main]

mod mem;
mod sys;

use core::panic::PanicInfo;

const MAX_CLIENTS: usize = 16;
const READ_BUF_SIZE: usize = 2048;

// Special epoll data tag so we can tell "new incoming connection" apart from
// "an existing client sent data or disconnected".
const LISTENER_TAG: u64 = u64::MAX;

#[derive(Copy, Clone)]
struct Client {
    fd: i32,
    active: bool,
    is_sse: bool,
    read_len: usize,
    read_buf: [u8; READ_BUF_SIZE],
}

impl Client {
    const EMPTY: Self = Self {
        fd: 0,
        active: false,
        is_sse: false,
        read_len: 0,
        read_buf: [0; READ_BUF_SIZE],
    };
}

/// Stored in static zero-initialized memory (.bss) rather than on the stack.
///
/// Memory trick: Linux doesn't actually allocate physical RAM for these 32 KB
/// until a client slot is touched — unused slots cost 0 KB of RAM!
static mut CLIENTS: [Client; MAX_CLIENTS] = [Client::EMPTY; MAX_CLIENTS];

#[unsafe(no_mangle)]
#[unsafe(naked)]
pub unsafe extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "and rsp, -16",
        "call {entry}",
        entry = sym entry,
    );
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    sys::exit(1)
}

extern "C" fn entry() -> ! {
    // SAFETY: Single-threaded program; `entry` is called once and owns CLIENTS exclusively.
    let clients = unsafe { &mut *core::ptr::addr_of_mut!(CLIENTS) };

    let listener = sys::socket(
        sys::AF_INET,
        sys::SOCK_STREAM | sys::SOCK_NONBLOCK | sys::SOCK_CLOEXEC,
        0,
    );
    if listener < 0 {
        sys::exit(1);
    }

    sys::setsockopt_i32(listener, sys::SOL_SOCKET, sys::SO_REUSEADDR, 1);

    let addr = sys::SockAddrIn {
        family: sys::AF_INET as u16,
        port: 8080u16.to_be(),
        addr: [127, 0, 0, 1],
        zero: [0; 8],
    };

    if sys::bind(listener, &addr) < 0 || sys::listen(listener, 16) < 0 {
        sys::exit(1);
    }

    // Create an epoll instance so Linux can wake us up when ANY socket is ready.
    let epfd = sys::epoll_create1(sys::EPOLL_CLOEXEC);
    if epfd < 0 {
        sys::exit(1);
    }

    let listen_ev = sys::EpollEvent {
        events: sys::EPOLLIN,
        data: LISTENER_TAG,
    };
    if sys::epoll_ctl(epfd, sys::EPOLL_CTL_ADD, listener, Some(&listen_ev)) < 0 {
        sys::exit(1);
    }

    let _ = sys::write(1, b"Listening on http://127.0.0.1:8080\n");

    let mut events = [sys::EpollEvent { events: 0, data: 0 }; MAX_CLIENTS];

    loop {
        // Sleep at 0% CPU until at least one socket has activity.
        let n = sys::epoll_wait(epfd, &mut events, -1);
        if n <= 0 {
            continue;
        }

        for ev in &events[..n as usize] {
            let tag = ev.data;
            let flags = ev.events;

            if tag == LISTENER_TAG {
                accept_new_clients(epfd, listener, clients);
            } else {
                let slot = tag as usize;
                if slot < MAX_CLIENTS && clients[slot].active {
                    if (flags & (sys::EPOLLERR | sys::EPOLLHUP | sys::EPOLLRDHUP)) != 0 {
                        close_client(epfd, &mut clients[slot]);
                    } else if (flags & sys::EPOLLIN) != 0 {
                        handle_client_read(epfd, clients, slot);
                    }
                }
            }
        }
    }
}

fn accept_new_clients(epfd: i32, listener: i32, clients: &mut [Client; MAX_CLIENTS]) {
    loop {
        let fd = sys::accept4(listener, sys::SOCK_NONBLOCK | sys::SOCK_CLOEXEC);
        if fd < 0 {
            // No more pending connections right now (EAGAIN)
            break;
        }

        // Find a free slot in our fixed-size client table
        let mut assigned = false;
        for (slot, client) in clients.iter_mut().enumerate() {
            if !client.active {
                client.fd = fd;
                client.active = true;
                client.is_sse = false;
                client.read_len = 0;

                let ev = sys::EpollEvent {
                    events: sys::EPOLLIN | sys::EPOLLRDHUP,
                    data: slot as u64,
                };
                if sys::epoll_ctl(epfd, sys::EPOLL_CTL_ADD, fd, Some(&ev)) < 0 {
                    client.active = false;
                    sys::close(fd);
                }
                assigned = true;
                break;
            }
        }

        // All slots full — politely close the extra connection
        if !assigned {
            sys::close(fd);
        }
    }
}

fn close_client(epfd: i32, client: &mut Client) {
    if client.active {
        let _ = sys::epoll_ctl(epfd, sys::EPOLL_CTL_DEL, client.fd, None);
        sys::close(client.fd);
        client.active = false;
        client.is_sse = false;
        client.read_len = 0;
    }
}

fn handle_client_read(epfd: i32, clients: &mut [Client; MAX_CLIENTS], slot: usize) {
    let fd = clients[slot].fd;
    let is_sse = clients[slot].is_sse;

    // If an SSE listener sent unexpected data or closed, read to detect EOF
    if is_sse {
        let mut tmp = [0u8; 64];
        let n = sys::read(fd, &mut tmp);
        if n <= 0 && n != sys::EAGAIN {
            close_client(epfd, &mut clients[slot]);
        }
        return;
    }

    let len = clients[slot].read_len;
    if len >= READ_BUF_SIZE {
        close_client(epfd, &mut clients[slot]);
        return;
    }

    let n = sys::read(fd, &mut clients[slot].read_buf[len..]);
    if n == 0 || (n < 0 && n != sys::EAGAIN) {
        close_client(epfd, &mut clients[slot]);
        return;
    }
    if n < 0 {
        return;
    }

    clients[slot].read_len += n as usize;
    let data = &clients[slot].read_buf[..clients[slot].read_len];

    // Wait until we've received the end of the HTTP headers ("\r\n\r\n")
    let Some(header_end) = find_subslice(data, b"\r\n\r\n") else {
        return;
    };

    let body_start = header_end + 4;

    if data.starts_with(b"GET /events ") {
        // Keep this connection open as a live Server-Sent Events stream!
        let sse_headers = b"HTTP/1.1 200 OK\r\n\
            Content-Type: text/event-stream\r\n\
            Cache-Control: no-cache\r\n\
            Connection: keep-alive\r\n\r\n\
            : connected\n\n";
        if sys::send_nosignal(fd, sse_headers) > 0 {
            clients[slot].is_sse = true;
            clients[slot].read_len = 0;
        } else {
            close_client(epfd, &mut clients[slot]);
        }
        return;
    }

    if data.starts_with(b"POST ") {
        // Copy the message body into a small stack buffer so we can broadcast it
        let body_len = (clients[slot].read_len - body_start).min(512);
        let mut msg = [0u8; 512];
        msg[..body_len].copy_from_slice(&clients[slot].read_buf[body_start..body_start + body_len]);

        let _ = sys::send_nosignal(
            fd,
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n",
        );
        close_client(epfd, &mut clients[slot]);

        // Broadcast to every connected SSE subscriber!
        broadcast_sse(epfd, clients, &msg[..body_len]);
        return;
    }

    // Default: simple "ok" response
    let _ = sys::send_nosignal(
        fd,
        b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n",
    );
    close_client(epfd, &mut clients[slot]);
}

fn broadcast_sse(epfd: i32, clients: &mut [Client; MAX_CLIENTS], msg: &[u8]) {
    for client in clients.iter_mut() {
        if client.active && client.is_sse {
            let ok1 = sys::send_nosignal(client.fd, b"data: ") >= 0;
            let ok2 = ok1 && sys::send_nosignal(client.fd, msg) >= 0;
            let ok3 = ok2 && sys::send_nosignal(client.fd, b"\n\n") >= 0;
            if !ok3 {
                close_client(epfd, client);
            }
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        if &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}
