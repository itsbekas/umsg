//! Zero-allocation runtime environment variable reader for `no_std` Linux.
//!
//! At process entry (`_start`), the Linux kernel places on the initial stack:
//!   [sp]               = argc
//!   [sp + 1 ..= argc]  = argv[0..argc]
//!   [sp + argc + 1]    = NULL
//!   [sp + argc + 2 ..] = envp[0..] ("KEY=VALUE\0"), terminated by NULL

pub struct Env {
    envp: *const *const u8,
}

impl Env {
    /// # Safety
    /// `sp` must be the original `rsp` passed by the Linux kernel to `_start`.
    pub unsafe fn from_initial_sp(sp: *const usize) -> Self {
        unsafe {
            let argc = *sp;
            let envp = sp.add(argc + 2) as *const *const u8;
            Self { envp }
        }
    }

    pub fn get(&self, key: &[u8]) -> Option<&'static [u8]> {
        let mut i = 0;
        loop {
            let ptr = unsafe { *self.envp.add(i) };
            if ptr.is_null() {
                return None;
            }
            let entry = unsafe { c_bytes(ptr) };
            if entry.len() > key.len() && &entry[..key.len()] == key && entry[key.len()] == b'=' {
                return Some(&entry[key.len() + 1..]);
            }
            i += 1;
        }
    }

    pub fn get_u64(&self, key: &[u8]) -> Option<u64> {
        let val = self.get(key)?;
        if val.is_empty() {
            return None;
        }
        let mut acc: u64 = 0;
        for &b in val {
            if !b.is_ascii_digit() {
                return None;
            }
            acc = acc.checked_mul(10)?.checked_add((b - b'0') as u64)?;
        }
        Some(acc)
    }
}

unsafe fn c_bytes(ptr: *const u8) -> &'static [u8] {
    let mut len = 0;
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    unsafe { core::slice::from_raw_parts(ptr, len) }
}
