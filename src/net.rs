use std::{
    io::{Error, Result},
    net::{SocketAddr, TcpListener, TcpStream},
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Stream;
use socket2::SockAddr;

use crate::io::*;

impl Async<TcpListener> {
    /// Accepts a TCP connection without blocking.
    ///
    /// This is an async version of [`TcpListener::accept()`]. Note that it
    /// gives you back a plain [`TcpStream`], not an `Async<TcpStream>`. This is
    /// so that you can send it to another thread first and then wrap it into
    /// `Async`, since `Async` is [`!Send`].
    ///
    /// # Examples
    ///
    /// ```
    /// use std::net::{TcpListener, TcpStream, SocketAddr};
    /// use futures::pin_mut;
    /// # use futures::task::LocalSpawnExt;
    /// use ground::Async;
    ///
    /// # executor::run(|sp| async move {
    /// let addr = "127.0.0.1:8000";
    /// # let addr = "127.0.0.1:0";
    /// let listener = TcpListener::bind(addr)?;
    /// # let addr = listener.local_addr()?;
    /// # let connect_task = sp.spawn_local(async move {
    /// #     let _ = Async::<TcpStream>::connect(addr).await.unwrap();
    /// # }).unwrap();
    /// let listener = Async::new(listener);
    /// pin_mut!(listener);
    ///
    /// let (connection, address): (TcpStream, SocketAddr) = listener.accept().await?;
    /// // Wrap it into `Async`, if we want to:
    /// let connection = Async::new(connection);
    /// # Ok::<(), std::io::Error>(())
    /// # }).unwrap();
    /// ```
    ///
    /// [`TcpListener::accept()`]: https://doc.rust-lang.org/std/net/struct.TcpListener.html#method.accept
    /// [`TcpStream`]: https://doc.rust-lang.org/std/net/struct.TcpStream.html
    /// [`!Send`]: struct.Async.html#impl-Send
    pub async fn accept(self: Pin<&mut Self>) -> Result<(TcpStream, SocketAddr)> {
        self.nonblocking(Interest::Read, |listener| listener.accept())
            .await
    }

    /// Returns a stream of incoming TCP connections without blocking.
    ///
    /// This is an async version of [`TcpListener::incoming()`]. This stream is
    /// endless, and will keep waiting for connections as long as you poll it
    /// for more items.
    ///
    /// Note that the items of this stream are of type [`TcpStream`], not
    /// `Async<TcpStream>`. This is so that you can send the items to other
    /// threads first and then wrap them into `Async`, since `Async` is
    /// [`!Send`].
    ///
    /// # Examples
    ///
    /// ```
    /// use std::net::TcpListener;
    /// use futures::{pin_mut, StreamExt};
    /// use ground::Async;
    ///
    /// # executor::run(|sp| async move {
    /// let addr = "127.0.0.1:8000";
    /// # let addr = "127.0.0.1:0";
    /// let listener = TcpListener::bind(addr)?;
    /// let listener = Async::new(listener);
    /// pin_mut!(listener);
    ///
    /// let mut incoming = listener.incoming();
    /// # // We're not actually going to run the following:
    /// # let _ = async {
    /// while let Some(connection) = incoming.next().await {
    ///    // Handle errors.
    ///    let connection = connection?;
    ///    // Wrap it into `Async`, if we want to:
    ///    let connection = Async::new(connection);
    /// }
    /// // This is an endless stream, so `next()` never returns `None`.
    /// unreachable!();
    /// # Ok::<(), std::io::Error>(())
    /// # };
    /// # Ok::<(), std::io::Error>(())
    /// # }).unwrap();
    /// ```
    ///
    /// [`TcpListener::incoming()`]: https://doc.rust-lang.org/std/net/struct.TcpListener.html#method.incoming
    /// [`TcpStream`]: https://doc.rust-lang.org/std/net/struct.TcpStream.html
    /// [`!Send`]: struct.Async.html#impl-Send
    pub fn incoming(self: Pin<&mut Self>) -> impl Stream<Item = Result<TcpStream>> + Unpin + '_ {
        Incoming { a: self }
    }
}

struct Incoming<'a> {
    a: Pin<&'a mut Async<TcpListener>>,
}

impl Unpin for Incoming<'_> {}

impl Stream for Incoming<'_> {
    type Item = Result<TcpStream>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let a: Pin<&mut Async<TcpListener>> = self.a.as_mut();
        match a.poll_nonblocking(cx, Interest::Read, |inner| inner.accept()) {
            Poll::Ready(Ok((stream, _addr))) => Poll::Ready(Some(Ok(stream))),
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Async<TcpStream> {
    /// Connects to the given TCP address without blocking.
    ///
    /// This is an async version of [`TcpStream::connect()`].
    ///
    /// Note that this method accepts an already resolved socket address, rather
    /// than performing address resolution itself. This is because it's
    /// non-trivial to perform address resolution without blocking, so this
    /// method leaves it up to the caller to figure that part out (making use of
    /// [`ground::dns`] might be one option).
    ///
    /// # Examples
    ///
    /// ```
    /// use std::{
    ///     io::{Error, ErrorKind},
    ///     net::{TcpStream, ToSocketAddrs},
    /// };
    /// use ground::Async;
    ///
    /// # executor::run(|_| async {
    /// // Resolve the address first. For this example, we use the `ToSocketAddrs`
    /// // implementation for `&str`, which is blocking.
    /// let error = Error::new(ErrorKind::Other, "failed to resolve address");
    /// let addr = "example.com:80".to_socket_addrs()?.next().ok_or(error)?;
    ///
    /// // Now, actually connect (non-blocking).
    /// let stream = Async::<TcpStream>::connect(addr).await?;
    /// # Ok::<(), std::io::Error>(())
    /// # }).unwrap();
    /// ```
    ///
    /// [`TcpStream::connect()`]: https://doc.rust-lang.org/std/net/struct.TcpStream.html#method.connect
    /// [`ground::dns`]: ../dns/index.html
    pub async fn connect(addr: impl Into<SocketAddr>) -> Result<Self> {
        // Create the socket.
        let addr: SocketAddr = addr.into();
        let domain = match addr {
            SocketAddr::V4(_) => socket2::Domain::IPV4,
            SocketAddr::V6(_) => socket2::Domain::IPV6,
        };
        let type_ = socket2::Type::STREAM.nonblocking().cloexec();
        let socket = socket2::Socket::new(domain, type_, Some(socket2::Protocol::TCP))?;
        let mut socket = Async::new(socket);
        // SAFETY: This is just pinning on the stack.
        let mut socket = unsafe { Pin::new_unchecked(&mut socket) };

        // Actually connect.
        let addr = SockAddr::from(addr);
        let mut called_connect = false;
        socket
            .as_mut()
            .nonblocking(Interest::Write, |socket| {
                if !called_connect {
                    // The first time this is called, we call `connect()` as
                    // expected, and then either receive a result immediately,
                    // or get back `EINPROGRESS`, which we convert to the usual
                    // `EAGAIN` that `nonblocking()` expects.
                    called_connect = true;
                    match socket.connect(&addr) {
                        Err(e) if e.raw_os_error() == Some(libc::EINPROGRESS) => {
                            Err(Error::from_raw_os_error(libc::EAGAIN))
                        }
                        anything_else => anything_else,
                    }
                } else {
                    // But the second time we get here, we don't call
                    // `connect()` again. We should either be connected already
                    // at this point, or the connection should have failed with
                    // an error. We call `take_error()` to find out which one it
                    // is.
                    match socket.take_error()? {
                        Some(err) => Err(err),
                        None => Ok(()),
                    }
                }
            })
            .await?;
        let stream = TcpStream::from(socket.take_inner());
        Ok(Async::new(stream))
    }
}
