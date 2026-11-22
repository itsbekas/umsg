#![no_std]
#![no_main]

mod config;
mod disk;
mod env;
mod fmt;
mod http;
mod mem;
mod server;
mod sse;
mod store;

use core::mem::MaybeUninit;
use core::net::Ipv4Addr;
use core::panic::PanicInfo;

use rustix::event::epoll::{self, CreateFlags, Event, EventData, EventFlags};
use rustix::io;
use rustix::net::{
    AddressFamily, SocketAddrV4, SocketFlags, SocketType, bind, listen, socket_with, sockopt,
};
use rustix::time::{ClockId, Timespec, clock_gettime};

use config::{
    DEFAULT_KEEPALIVE_SECS, DEFAULT_LOG_PATH, DEFAULT_PORT, MAX_CLIENTS, MAX_SSE_CLIENTS,
    READ_TIMEOUT_SECS,
};
use disk::DiskLog;
use env::Env;
use fmt::BufWriter;
use server::{HttpClient, accept_new_clients, close_http_client, handle_client_read};
use sse::{SseClient, close_sse_client, send_sse_keepalive};
use store::Store;

const LISTENER_TAG: u64 = u64::MAX;

/// Static zero-initialized storage (.bss).
/// Linux lazy-pages .bss so unused client slots and unused history entries
/// consume 0 KB of physical RAM until written.
static mut HTTP_CLIENTS: [HttpClient; MAX_CLIENTS] = [const { HttpClient::EMPTY }; MAX_CLIENTS];
static mut SSE_CLIENTS: [SseClient; MAX_SSE_CLIENTS] =
    [const { SseClient::EMPTY }; MAX_SSE_CLIENTS];
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

    // SAFETY: Single-threaded program; `entry` is called once and owns all statics exclusively.
    let http_clients = unsafe { &mut *core::ptr::addr_of_mut!(HTTP_CLIENTS) };
    let sse_clients = unsafe { &mut *core::ptr::addr_of_mut!(SSE_CLIENTS) };
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
                accept_new_clients(&epfd, &listener, http_clients, now);
            } else {
                let idx = tag as usize;
                if idx < MAX_CLIENTS {
                    if http_clients[idx].fd.is_some() {
                        if flags.intersects(EventFlags::ERR | EventFlags::HUP | EventFlags::RDHUP) {
                            close_http_client(&epfd, &mut http_clients[idx]);
                        } else if flags.contains(EventFlags::IN) {
                            handle_client_read(
                                &epfd,
                                http_clients,
                                sse_clients,
                                store,
                                &disk,
                                auth_token,
                                idx,
                            );
                        }
                    }
                } else {
                    let sse_slot = idx - MAX_CLIENTS;
                    if sse_slot < MAX_SSE_CLIENTS
                        && sse_clients[sse_slot].fd.is_some()
                        && flags.intersects(EventFlags::ERR | EventFlags::HUP | EventFlags::RDHUP)
                    {
                        close_sse_client(&epfd, &mut sse_clients[sse_slot]);
                    }
                }
            }
        }

        // Reap slow/idle HTTP connections (hard deadline defeats trickle Slowloris)
        for client in http_clients.iter_mut() {
            if client.fd.is_some() && now.saturating_sub(client.connected_at) >= READ_TIMEOUT_SECS {
                close_http_client(&epfd, client);
            }
        }

        // Monotonic SSE keepalive (cannot be starved by activity on other sockets)
        if now.saturating_sub(last_keepalive) >= keepalive_secs {
            send_sse_keepalive(&epfd, sse_clients);
            last_keepalive = now;
        }
    }
}

fn monotonic_secs() -> i64 {
    clock_gettime(ClockId::Monotonic).tv_sec
}
