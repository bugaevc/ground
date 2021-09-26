use std::{
    io::Result,
    net::{TcpListener, TcpStream},
};

use futures::{
    executor::LocalSpawner,
    io::{AsyncReadExt, AsyncWriteExt},
    pin_mut,
    stream::StreamExt,
    task::LocalSpawnExt,
};

use ground::Async;

async fn echo(stream: Async<TcpStream>, index: usize) -> Result<()> {
    pin_mut!(stream);

    let mut buf = [0; 512];
    loop {
        let nread = stream.read(&mut buf).await?;
        eprintln!("Read {} bytes from stream no {}", nread, index);
        if nread == 0 {
            return Ok(());
        }
        stream.write_all(&buf[..nread]).await?;
    }
}

async fn async_main(spawner: LocalSpawner) -> Result<()> {
    let addr = "127.0.0.1:8000";
    let listener = TcpListener::bind(addr)?;
    let listener = Async::new(listener);
    pin_mut!(listener);
    let incoming = listener.incoming();
    pin_mut!(incoming);
    eprintln!("Listening on {}", addr);

    let mut index = 0;
    while let Some(stream) = incoming.next().await {
        let stream = Async::new(stream?);
        index += 1;
        eprintln!("Accepted stream no {}", index);

        spawner
            .spawn_local(async move {
                if let Err(err) = echo(stream, index).await {
                    eprintln!("Failed to echo: {}", err);
                }
            })
            .expect("Failed to spawn");
    }
    unreachable!()
}

fn main() -> Result<()> {
    executor::run(async_main)
}
