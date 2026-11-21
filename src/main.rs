#![no_std]
#![no_main]

mod mem;

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

const MAX_CLIENTS: usize = 16;
const READ_BUF_SIZE: usize = 2048;

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

/// Stored in static zero-initialized memory (.bss) rather than on the stack.
/// Linux lazy-pages .bss, so unused client slots cost 0 KB of physical RAM.
static mut CLIENTS: [Client; MAX_CLIENTS] = [const { Client::EMPTY }; MAX_CLIENTS];

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
    // SAFETY: Single-threaded program; `entry` is called once and owns CLIENTS exclusively.
    let clients = unsafe { &mut *core::ptr::addr_of_mut!(CLIENTS) };

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
    let _ = io::write(unsafe { rustix::stdio::stdout() }, b"Listening on http://127.0.0.1:8080\n");

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
                        handle_client_read(&epfd, clients, slot);
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
                // If epoll::add failed, `fd` is dropped here and automatically closed!
                break;
            }
        }
        // If no free slot was found, `fd` is dropped here and automatically closed!
    }
}

fn close_client(epfd: &OwnedFd, client: &mut Client) {
    if let Some(fd) = client.fd.take() {
        let _ = epoll::delete(epfd, &fd);
        // `fd` is dropped right here -> automatically closed by Linux
    }
    client.is_sse = false;
    client.read_len = 0;
}

fn handle_client_read(epfd: &OwnedFd, clients: &mut [Client; MAX_CLIENTS], slot: usize) {
    let is_sse = clients[slot].is_sse;

    if is_sse {
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
    let data = &clients[slot].read_buf[..clients[slot].read_len];

    let Some(header_end) = find_subslice(data, b"\r\n\r\n") else {
        return;
    };

    let body_start = header_end + 4;

    if data.starts_with(b"GET /events ") {
        let sse_headers = b"HTTP/1.1 200 OK\r\n\
            Content-Type: text/event-stream\r\n\
            Cache-Control: no-cache\r\n\
            Connection: keep-alive\r\n\r\n\
            : connected\n\n";
        let ok = clients[slot]
            .fd
            .as_ref()
            .is_some_and(|fd| send_all(fd, sse_headers));
        if ok {
            clients[slot].is_sse = true;
            clients[slot].read_len = 0;
        } else {
            close_client(epfd, &mut clients[slot]);
        }
        return;
    }

    if data.starts_with(b"POST ") {
        let body_len = (clients[slot].read_len - body_start).min(512);
        let mut msg = [0u8; 512];
        msg[..body_len]
            .copy_from_slice(&clients[slot].read_buf[body_start..body_start + body_len]);

        if let Some(fd) = clients[slot].fd.as_ref() {
            let _ = send_all(
                fd,
                b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n",
            );
        }
        close_client(epfd, &mut clients[slot]);

        broadcast_sse(epfd, clients, &msg[..body_len]);
        return;
    }

    if let Some(fd) = clients[slot].fd.as_ref() {
        let _ = send_all(
            fd,
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n",
        );
    }
    close_client(epfd, &mut clients[slot]);
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

fn broadcast_sse(epfd: &OwnedFd, clients: &mut [Client; MAX_CLIENTS], msg: &[u8]) {
    for client in clients.iter_mut() {
        if client.is_sse {
            let ok = client.fd.as_ref().is_some_and(|fd| {
                send_all(fd, b"data: ") && send_all(fd, msg) && send_all(fd, b"\n\n")
            });
            if !ok {
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
