use std::{
    io::Result,
    net::{SocketAddr, TcpStream, ToSocketAddrs},
};

use futures::{
    future::try_join_all,
    io::{AsyncReadExt, AsyncWriteExt},
    pin_mut,
};

use ground::Async;

async fn do_http(addr: SocketAddr) -> Result<()> {
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
    let _size = stream.read(&mut buf).await?;
    Ok(())
}

async fn async_main() -> Result<()> {
    let addr = "example.com:80";
    // Note: the address resolution here is blocking :(
    let addr = match addr.to_socket_addrs()?.next() {
        Some(addr) => addr,
        None => panic!("Failed to resolve {}", addr),
    };

    let mut futures = Vec::new();
    for i in 0..100 {
        let addr = addr.clone();
        let future = async move {
            eprintln!("Making request #{}", i);
            let r = do_http(addr).await;
            eprintln!("Completed request #{}", i);
            r
        };
        futures.push(future);
    }

    try_join_all(futures).await?;
    Ok(())
}

fn main() -> Result<()> {
    executor::run(|_| async_main())
}
