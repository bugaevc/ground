use std::{
    cell::RefCell,
    collections::hash_map::{Entry, HashMap},
    io::Result,
    net::{TcpListener, TcpStream},
    rc::Rc,
};

use futures::{
    executor::LocalSpawner,
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    pin_mut,
    sink::SinkExt,
    stream::StreamExt,
    task::LocalSpawnExt,
};

use ground::Async;

#[derive(Debug)]
struct Message {
    sender: String,
    text: String,
}

type Sender = futures::channel::mpsc::Sender<Message>;
type Receiver = futures::channel::mpsc::Receiver<Message>;

#[derive(Debug)]
struct Peers {
    channels: HashMap<String, Sender>,
}

async fn send_loop(mut receiver: Receiver, connection: impl AsyncWrite) {
    pin_mut!(connection);
    while let Some(message) = receiver.next().await {
        let data = format!("from {}: {}\n", message.sender, message.text);
        let data = data.as_bytes();
        if let Err(err) = connection.write_all(data).await {
            eprintln!("Error writing: {}", err);
            return;
        }
    }
}

async fn recv_loop(connection: impl AsyncBufRead, name: String, peers: Rc<RefCell<Peers>>) {
    let lines = connection.lines();
    pin_mut!(lines);
    while let Some(line) = lines.next().await {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                eprintln!("Error reading: {}", err);
                return;
            }
        };
        // E.g. "alice,bob: hi!".
        let mut parts = line.splitn(2, ':');
        let (users, text) = match (parts.next(), parts.next()) {
            (Some(users), Some(text)) => (users, text.trim()),
            _ => {
                eprintln!("Malformed message: {}", line);
                continue;
            }
        };
        for user in users.split(',') {
            let mut sender: Sender = {
                let peers = peers.borrow();
                match peers.channels.get(user) {
                    Some(sender) => sender.clone(),
                    None => {
                        eprintln!("No such user {}", user);
                        continue;
                    }
                }
            };

            match sender
                .send(Message {
                    sender: name.clone(),
                    text: text.to_string(),
                })
                .await
            {
                Ok(()) => {}
                Err(_) => {
                    eprintln!(
                        "User {} went away while we were trying to send it a message",
                        user
                    );
                    continue;
                }
            };
        }
    }
}

async fn handle_client(connection: TcpStream, spawner: LocalSpawner, peers: Rc<RefCell<Peers>>) {
    let connection = Async::new(connection);
    let (read_half, write_half) = connection.split();
    let mut buf_read = BufReader::new(read_half);

    // First line: the name.
    let mut name = String::new();
    if let Err(err) = buf_read.read_line(&mut name).await {
        eprintln!("Error reading name: {}", err);
    }
    name = name.trim().to_string();

    {
        let mut peers = peers.borrow_mut();
        match peers.channels.entry(name.clone()) {
            Entry::Occupied(_) => {
                eprintln!("Name {} already claimed", name);
                return;
            }
            Entry::Vacant(entry) => {
                let (sender, receiver) = futures::channel::mpsc::channel(16);
                entry.insert(sender);
                spawner
                    .spawn_local(send_loop(receiver, write_half))
                    .expect("Failed to spawn");
            }
        }
    }

    recv_loop(buf_read, name.clone(), peers.clone()).await;

    let mut peers = peers.borrow_mut();
    let sender = peers.channels.remove(&name);
    debug_assert!(sender.is_some());
}

async fn async_main(spawner: LocalSpawner) -> Result<()> {
    let peers = Peers {
        channels: HashMap::new(),
    };
    let peers = Rc::new(RefCell::new(peers));

    let addr = "127.0.0.1:8000";
    let listener = TcpListener::bind(addr)?;
    let listener = Async::new(listener);
    pin_mut!(listener);
    let incoming = listener.incoming();
    pin_mut!(incoming);
    eprintln!("Listening on {}", addr);

    while let Some(stream) = incoming.next().await {
        let stream = stream?;
        let task = handle_client(stream, spawner.clone(), peers.clone());
        spawner.spawn_local(task).expect("Failed to spawn");
    }

    unreachable!()
}

fn main() -> Result<()> {
    executor::run(async_main)
}
