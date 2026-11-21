use std::io::{Read, Write};
use std::net::TcpListener;

fn main() {
    let listener = TcpListener::bind("127.0.0.1:8080").expect("failed to bind");
    println!("Listening on http://127.0.0.1:8080");

    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };

        let mut buf = [0u8; 512];
        let _ = stream.read(&mut buf);

        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n";
        let _ = stream.write_all(response);
    }
}
