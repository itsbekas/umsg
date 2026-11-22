//! Tiny replacements for the 5 memory primitives LLVM expects from libc.
//!
//! Because we have zero libc, Rust's slice comparisons (`a == b`) and copies
//! (`copy_from_slice`) need these symbols at link time. We use volatile reads
//! and writes so the compiler doesn't try to optimize `memcpy` into a call to
//! `memcpy`!

use core::ffi::c_void;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    let d = dst as *mut u8;
    let s = src as *const u8;
    let mut i = 0;
    while i < n {
        unsafe {
            d.add(i).write_volatile(s.add(i).read_volatile());
        }
        i += 1;
    }
    dst
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memmove(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    let d = dst as *mut u8;
    let s = src as *const u8;
    if (d as usize) <= (s as usize) {
        let mut i = 0;
        while i < n {
            unsafe {
                d.add(i).write_volatile(s.add(i).read_volatile());
            }
            i += 1;
        }
    } else {
        let mut i = n;
        while i > 0 {
            i -= 1;
            unsafe {
                d.add(i).write_volatile(s.add(i).read_volatile());
            }
        }
    }
    dst
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(dst: *mut c_void, val: i32, n: usize) -> *mut c_void {
    let d = dst as *mut u8;
    let byte = val as u8;
    let mut i = 0;
    while i < n {
        unsafe {
            d.add(i).write_volatile(byte);
        }
        i += 1;
    }
    dst
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    let x = a as *const u8;
    let y = b as *const u8;
    let mut i = 0;
    while i < n {
        let bx = unsafe { x.add(i).read_volatile() };
        let by = unsafe { y.add(i).read_volatile() };
        if bx != by {
            return (bx as i32) - (by as i32);
        }
        i += 1;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn bcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    unsafe { memcmp(a, b, n) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strlen(s: *const core::ffi::c_char) -> usize {
    let mut i = 0;
    while unsafe { s.add(i).read_volatile() } != 0 {
        i += 1;
    }
    i
}
