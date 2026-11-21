#![no_std]
#![no_main]

mod sys;

use core::panic::PanicInfo;

/// Linux jumps directly here when starting our program (no C runtime, no std).
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
    let listener = sys::socket(sys::AF_INET, sys::SOCK_STREAM, 0);
    if listener < 0 {
        sys::exit(1);
    }

    // Allow immediate restart without waiting for TIME_WAIT
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

    let _ = sys::write(1, b"Listening on http://127.0.0.1:8080\n");

    let mut buf = [0u8; 512];
    let response = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n";

    loop {
        let client = sys::accept(listener);
        if client < 0 {
            continue;
        }

        let _ = sys::read(client, &mut buf);
        let _ = sys::write(client, response);
        sys::close(client);
    }
}
