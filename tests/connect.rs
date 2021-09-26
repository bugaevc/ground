use std::net::{TcpStream, ToSocketAddrs};

use ground::Async;

#[test]
fn connect_can_fail() {
    let addr = "127.0.35.0:80"
        .to_socket_addrs()
        .expect("Failed to parse address")
        .next()
        .expect("Address parsed to nothing");

    executor::run(move |_| async move {
        match Async::<TcpStream>::connect(addr).await {
            Ok(_) => panic!("Connecting to invalid address should fail"),
            Err(e) => eprintln!("It failed, good. Error is {}", e),
        }
    });
}
