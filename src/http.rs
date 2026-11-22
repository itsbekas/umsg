//! Zero-allocation HTTP/1.1 request parser.
//!
//! Instead of allocating `String`s or `HashMap`s for headers, every field in
//! `Request<'a>` is just a borrowed slice (`&'a [u8]`) pointing directly into
//! the client's existing `read_buf`.

pub enum Method {
    Get,
    Post,
    Other,
}

pub struct Request<'a> {
    pub method: Method,
    pub path: &'a [u8],
    pub title: &'a [u8],
    pub priority: u8, // 1=min, 2=low, 3=default, 4=high, 5=urgent
    pub body: &'a [u8],
}

pub enum ParseStatus<'a> {
    Incomplete,
    Invalid,
    Complete(Request<'a>),
}

/// Parses an HTTP/1.1 request in-place from `buf`.
/// Returns `Incomplete` if headers or the full `Content-Length` body haven't arrived yet.
pub fn parse_request(buf: &[u8]) -> ParseStatus<'_> {
    let Some(header_end) = find_subslice(buf, b"\r\n\r\n") else {
        return ParseStatus::Incomplete;
    };

    let head = &buf[..header_end];
    let body_start = header_end + 4;

    // Parse request line: "METHOD /path HTTP/1.1"
    let first_line_end = find_subslice(head, b"\r\n").unwrap_or(head.len());
    let req_line = &head[..first_line_end];

    let Some(sp1) = find_byte(req_line, b' ') else {
        return ParseStatus::Invalid;
    };
    let rest = &req_line[sp1 + 1..];
    let Some(sp2) = find_byte(rest, b' ') else {
        return ParseStatus::Invalid;
    };

    let method = match &req_line[..sp1] {
        b"GET" => Method::Get,
        b"POST" => Method::Post,
        _ => Method::Other,
    };
    let path = &rest[..sp2];

    // Scan headers without allocating
    let mut content_length: usize = 0;
    let mut title: &[u8] = b"";
    let mut priority: u8 = 3;

    let mut pos = first_line_end;
    while pos + 2 <= head.len() {
        pos += 2; // skip "\r\n"
        let line_end = find_subslice(&head[pos..], b"\r\n")
            .map(|i| pos + i)
            .unwrap_or(head.len());
        let line = &head[pos..line_end];

        if let Some(colon) = find_byte(line, b':') {
            let name = &line[..colon];
            let val = trim_ascii(&line[colon + 1..]);

            if eq_ignore_ascii_case(name, b"content-length") {
                if let Some(n) = parse_usize(val) {
                    content_length = n;
                }
            } else if eq_ignore_ascii_case(name, b"title") || eq_ignore_ascii_case(name, b"x-title")
            {
                title = val;
            } else if eq_ignore_ascii_case(name, b"priority")
                || eq_ignore_ascii_case(name, b"x-priority")
            {
                priority = parse_priority(val);
            }
        }

        pos = line_end;
    }

    let available_body = buf.len() - body_start;
    if available_body < content_length {
        return ParseStatus::Incomplete;
    }

    let body = if content_length > 0 {
        &buf[body_start..body_start + content_length]
    } else {
        &buf[body_start..]
    };

    ParseStatus::Complete(Request {
        method,
        path,
        title,
        priority,
        body,
    })
}

fn parse_priority(val: &[u8]) -> u8 {
    if val == b"1" || eq_ignore_ascii_case(val, b"min") {
        1
    } else if val == b"2" || eq_ignore_ascii_case(val, b"low") {
        2
    } else if val == b"4" || eq_ignore_ascii_case(val, b"high") {
        4
    } else if val == b"5"
        || eq_ignore_ascii_case(val, b"urgent")
        || eq_ignore_ascii_case(val, b"max")
    {
        5
    } else {
        3
    }
}

fn find_byte(haystack: &[u8], needle: u8) -> Option<usize> {
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

pub fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
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

fn trim_ascii(mut s: &[u8]) -> &[u8] {
    while let Some(&first) = s.first() {
        if first == b' ' || first == b'\t' {
            s = &s[1..];
        } else {
            break;
        }
    }
    while let Some(&last) = s.last() {
        if last == b' ' || last == b'\t' || last == b'\r' || last == b'\n' {
            s = &s[..s.len() - 1];
        } else {
            break;
        }
    }
    s
}

fn eq_ignore_ascii_case(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i].to_ascii_lowercase() != b[i].to_ascii_lowercase() {
            return false;
        }
        i += 1;
    }
    true
}

fn parse_usize(s: &[u8]) -> Option<usize> {
    if s.is_empty() {
        return None;
    }
    let mut acc: usize = 0;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add((b - b'0') as usize)?;
    }
    Some(acc)
}
