//! Minimal, direct Linux x86_64 system calls — no libc, no std.
//!
//! On x86_64 Linux, a syscall puts its number in `rax` and up to 6 arguments
//! in `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9`, then executes the `syscall`
//! CPU instruction. The kernel returns the result (or a negative error code)
//! in `rax`.

use core::arch::asm;

// Linux x86_64 syscall numbers (from /usr/include/asm/unistd_64.h)
const SYS_READ: usize = 0;
const SYS_WRITE: usize = 1;
const SYS_CLOSE: usize = 3;
const SYS_SOCKET: usize = 41;
const SYS_SENDTO: usize = 44;
const SYS_BIND: usize = 49;
const SYS_LISTEN: usize = 50;
const SYS_SETSOCKOPT: usize = 54;
const SYS_EPOLL_WAIT: usize = 232;
const SYS_EPOLL_CTL: usize = 233;
const SYS_EXIT_GROUP: usize = 231;
const SYS_ACCEPT4: usize = 288;
const SYS_EPOLL_CREATE1: usize = 291;

// Socket constants
pub const AF_INET: usize = 2;
pub const SOCK_STREAM: usize = 1;
pub const SOCK_NONBLOCK: usize = 0x800;
pub const SOCK_CLOEXEC: usize = 0x80000;
pub const SOL_SOCKET: usize = 1;
pub const SO_REUSEADDR: usize = 2;
pub const MSG_NOSIGNAL: usize = 0x4000;

// Epoll constants
pub const EPOLL_CLOEXEC: usize = 0x80000;
pub const EPOLL_CTL_ADD: i32 = 1;
pub const EPOLL_CTL_DEL: i32 = 2;
pub const EPOLLIN: u32 = 0x001;
pub const EPOLLERR: u32 = 0x008;
pub const EPOLLHUP: u32 = 0x010;
pub const EPOLLRDHUP: u32 = 0x2000;

// Common error codes (negated as returned by Linux syscalls)
pub const EAGAIN: isize = -11;

/// IPv4 socket address in the exact byte layout the Linux kernel expects.
#[repr(C)]
pub struct SockAddrIn {
    pub family: u16,
    pub port: u16,     // big-endian (network byte order)
    pub addr: [u8; 4], // e.g. [127, 0, 0, 1]
    pub zero: [u8; 8],
}

/// Epoll event layout on x86_64 Linux (must be packed: 12 bytes, no padding!).
#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EpollEvent {
    pub events: u32,
    pub data: u64,
}

#[inline(always)]
unsafe fn syscall1(num: usize, a1: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") num => ret,
            in("rdi") a1,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
unsafe fn syscall3(num: usize, a1: usize, a2: usize, a3: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") num => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
unsafe fn syscall4(num: usize, a1: usize, a2: usize, a3: usize, a4: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") num => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
unsafe fn syscall5(
    num: usize,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") num => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            in("r8") a5,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
unsafe fn syscall6(
    num: usize,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") num => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            in("r8") a5,
            in("r9") a6,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

pub fn read(fd: i32, buf: &mut [u8]) -> isize {
    unsafe { syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) }
}

pub fn write(fd: i32, buf: &[u8]) -> isize {
    unsafe { syscall3(SYS_WRITE, fd as usize, buf.as_ptr() as usize, buf.len()) }
}

/// Write to a socket without crashing (SIGPIPE) if the client already disconnected.
pub fn send_nosignal(fd: i32, buf: &[u8]) -> isize {
    unsafe {
        syscall6(
            SYS_SENDTO,
            fd as usize,
            buf.as_ptr() as usize,
            buf.len(),
            MSG_NOSIGNAL,
            0,
            0,
        )
    }
}

pub fn close(fd: i32) -> isize {
    unsafe { syscall1(SYS_CLOSE, fd as usize) }
}

pub fn socket(domain: usize, ty: usize, protocol: usize) -> i32 {
    unsafe { syscall3(SYS_SOCKET, domain, ty, protocol) as i32 }
}

pub fn setsockopt_i32(fd: i32, level: usize, optname: usize, val: i32) -> isize {
    unsafe {
        syscall5(
            SYS_SETSOCKOPT,
            fd as usize,
            level,
            optname,
            &val as *const i32 as usize,
            core::mem::size_of::<i32>(),
        )
    }
}

pub fn bind(fd: i32, addr: &SockAddrIn) -> isize {
    unsafe {
        syscall3(
            SYS_BIND,
            fd as usize,
            addr as *const SockAddrIn as usize,
            core::mem::size_of::<SockAddrIn>(),
        )
    }
}

pub fn listen(fd: i32, backlog: i32) -> isize {
    unsafe { syscall3(SYS_LISTEN, fd as usize, backlog as usize, 0) }
}

pub fn accept4(fd: i32, flags: usize) -> i32 {
    unsafe { syscall4(SYS_ACCEPT4, fd as usize, 0, 0, flags) as i32 }
}

pub fn epoll_create1(flags: usize) -> i32 {
    unsafe { syscall1(SYS_EPOLL_CREATE1, flags) as i32 }
}

pub fn epoll_ctl(epfd: i32, op: i32, fd: i32, event: Option<&EpollEvent>) -> isize {
    let ptr = match event {
        Some(ev) => ev as *const EpollEvent as usize,
        None => 0,
    };
    unsafe { syscall4(SYS_EPOLL_CTL, epfd as usize, op as usize, fd as usize, ptr) }
}

pub fn epoll_wait(epfd: i32, events: &mut [EpollEvent], timeout_ms: i32) -> isize {
    unsafe {
        syscall4(
            SYS_EPOLL_WAIT,
            epfd as usize,
            events.as_mut_ptr() as usize,
            events.len(),
            timeout_ms as usize,
        )
    }
}

pub fn exit(code: i32) -> ! {
    unsafe {
        syscall1(SYS_EXIT_GROUP, code as usize);
        core::hint::unreachable_unchecked()
    }
}
