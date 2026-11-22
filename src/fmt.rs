//! Tiny stack-buffer formatter (`no_std`, zero heap, no `core::fmt` bloat).

pub struct BufWriter<'a> {
    buf: &'a mut [u8],
    pub len: usize,
}

impl<'a> BufWriter<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }

    pub fn push_byte(&mut self, b: u8) {
        if self.len < self.buf.len() {
            self.buf[self.len] = b;
            self.len += 1;
        }
    }

    pub fn push_bytes(&mut self, s: &[u8]) {
        let avail = self.buf.len() - self.len;
        let n = s.len().min(avail);
        self.buf[self.len..self.len + n].copy_from_slice(&s[..n]);
        self.len += n;
    }

    pub fn push_u64(&mut self, mut val: u64) {
        if val == 0 {
            self.push_byte(b'0');
            return;
        }
        let mut tmp = [0u8; 20];
        let mut i = 0;
        while val > 0 {
            tmp[i] = b'0' + (val % 10) as u8;
            val /= 10;
            i += 1;
        }
        while i > 0 {
            i -= 1;
            self.push_byte(tmp[i]);
        }
    }

    pub fn push_i64(&mut self, val: i64) {
        if val < 0 {
            self.push_byte(b'-');
            self.push_u64(val.unsigned_abs());
        } else {
            self.push_u64(val as u64);
        }
    }

    pub fn push_json_escaped(&mut self, s: &[u8]) {
        for &b in s {
            match b {
                b'"' => self.push_bytes(b"\\\""),
                b'\\' => self.push_bytes(b"\\\\"),
                b'\n' => self.push_bytes(b"\\n"),
                b'\r' => self.push_bytes(b"\\r"),
                b'\t' => self.push_bytes(b"\\t"),
                0x00..=0x1f => {} // strip other control bytes
                _ => self.push_byte(b),
            }
        }
    }
}
