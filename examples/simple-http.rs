use std::{
    io::Result,
    net::{TcpStream, ToSocketAddrs},
};

use futures::{
    io::{AsyncReadExt, AsyncWriteExt},
    pin_mut,
};

use ground::Async;

async fn async_main() -> Result<()> {
    let addr = "example.com:80";
    // Note: the address resolution here is blocking :(
    let addr = match addr.to_socket_addrs()?.next() {
        Some(addr) => addr,
        None => panic!("Failed to resolve {}", addr),
    };

    // Connect.
    let stream = Async::<TcpStream>::connect(addr).await?;
    pin_mut!(stream);

    // Write.
    stream
        .write_all(
            b"GET / HTTP/1.1\r\n\
                       Host: example.com\r\n\r\n",
        )
        .await?;
    stream.flush().await?;

    // Read.
    let mut buf = [0; 512];
    let size = stream.read(&mut buf).await?;
    let buf = &buf[..size];

    // Display what we've read.
    if let Ok(s) = std::str::from_utf8(&buf) {
        println!("{}", s);
    } else {
        println!("{:?}", buf);
    }
    Ok(())
}

fn main() -> Result<()> {
    executor::run(|_| async_main())
}
