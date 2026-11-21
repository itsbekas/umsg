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
const SYS_ACCEPT: usize = 43;
const SYS_BIND: usize = 49;
const SYS_LISTEN: usize = 50;
const SYS_SETSOCKOPT: usize = 54;
const SYS_EXIT_GROUP: usize = 231;

// Socket constants
pub const AF_INET: usize = 2;
pub const SOCK_STREAM: usize = 1;
pub const SOL_SOCKET: usize = 1;
pub const SO_REUSEADDR: usize = 2;

/// IPv4 socket address in the exact byte layout the Linux kernel expects.
#[repr(C)]
pub struct SockAddrIn {
    pub family: u16,
    pub port: u16,     // big-endian (network byte order)
    pub addr: [u8; 4], // e.g. [127, 0, 0, 1]
    pub zero: [u8; 8],
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

pub fn read(fd: i32, buf: &mut [u8]) -> isize {
    unsafe { syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) }
}

pub fn write(fd: i32, buf: &[u8]) -> isize {
    unsafe { syscall3(SYS_WRITE, fd as usize, buf.as_ptr() as usize, buf.len()) }
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

pub fn accept(fd: i32) -> i32 {
    unsafe { syscall3(SYS_ACCEPT, fd as usize, 0, 0) as i32 }
}

pub fn exit(code: i32) -> ! {
    unsafe {
        syscall1(SYS_EXIT_GROUP, code as usize);
        core::hint::unreachable_unchecked()
    }
}
