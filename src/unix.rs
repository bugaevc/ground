use std::{
    io::Result,
    os::unix::net::{SocketAddr, UnixListener, UnixStream},
    path::Path,
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Stream;
use socket2::SockAddr;

use crate::io::*;

impl Async<UnixListener> {
    /// Accepts a Unix connection without blocking.
    ///
    /// This is an async version of [`UnixListener::accept()`]. Note that it
    /// gives you back a plain [`UnixStream`], not an `Async<UnixStream>`. This
    /// is so that you can send it to another thread first and then wrap it into
    /// `Async`, since `Async` is [`!Send`].
    ///
    /// # Examples
    ///
    /// ```
    /// use std::os::unix::net::{UnixListener, UnixStream, SocketAddr};
    /// use futures::pin_mut;
    /// # use futures::task::LocalSpawnExt;
    /// use ground::Async;
    ///
    /// # executor::run(|sp| async move {
    /// let path = "/tmp/ground-demo";
    /// let listener = UnixListener::bind(path)?;
    /// # let connect_task = sp.spawn_local(async move {
    /// #     let _ = Async::<UnixStream>::connect(path).await.unwrap();
    /// # }).unwrap();
    /// let listener = Async::new(listener);
    /// pin_mut!(listener);
    ///
    /// let (connection, address): (UnixStream, SocketAddr) = listener.accept().await?;
    /// // Wrap it into `Async`, if we want to:
    /// let connection = Async::new(connection);
    /// # std::fs::remove_file(path)
    /// # }).unwrap();
    /// ```
    ///
    /// [`UnixListener::accept()`]: https://doc.rust-lang.org/std/os/unix/net/struct.UnixListener.html#method.accept
    /// [`UnixStream`]: https://doc.rust-lang.org/std/os/unix/net/struct.UnixStream.html
    /// [`!Send`]: struct.Async.html#impl-Send
    pub async fn accept(self: Pin<&mut Self>) -> Result<(UnixStream, SocketAddr)> {
        self.nonblocking(Interest::Read, |listener| listener.accept())
            .await
    }

    /// Returns a stream of incoming Unix connections without blocking.
    ///
    /// This is an async version of [`UnixListener::incoming()`]. This stream is
    /// endless, and will keep waiting for connections as long as you poll it
    /// for more items.
    ///
    /// Note that the items of this stream are of type [`UnixStream`], not
    /// `Async<UnixStream>`. This is so that you can send the items to other
    /// threads first and then wrap them into `Async`, since `Async` is
    /// [`!Send`].
    ///
    /// # Examples
    ///
    /// ```
    /// use std::os::unix::net::UnixListener;
    /// use futures::{pin_mut, StreamExt};
    /// use ground::Async;
    ///
    /// # executor::run(|sp| async move {
    /// let path = "/tmp/ground-demo";
    /// let listener = UnixListener::bind(path)?;
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
    /// # std::fs::remove_file(path)
    /// # }).unwrap();
    /// ```
    ///
    /// [`UnixListener::incoming()`]: https://doc.rust-lang.org/std/os/unix/net/struct.UnixListener.html#method.incoming
    /// [`UnixStream`]: https://doc.rust-lang.org/std/os/unix/net/struct.UnixStream.html
    /// [`!Send`]: struct.Async.html#impl-Send
    pub fn incoming(self: Pin<&mut Self>) -> impl Stream<Item = Result<UnixStream>> + Unpin + '_ {
        Incoming { a: self }
    }
}

struct Incoming<'a> {
    a: Pin<&'a mut Async<UnixListener>>,
}

impl Unpin for Incoming<'_> {}

impl Stream for Incoming<'_> {
    type Item = Result<UnixStream>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let a: Pin<&mut Async<UnixListener>> = self.a.as_mut();
        match a.poll_nonblocking(cx, Interest::Read, |inner| inner.accept()) {
            Poll::Ready(Ok((stream, _addr))) => Poll::Ready(Some(Ok(stream))),
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Async<UnixStream> {
    /// Connects to a Unix socket without blocking.
    ///
    /// This is an async version of [`UnixStream::connect()`].
    ///
    /// # Examples
    ///
    /// ```
    /// use std::os::unix::net::UnixStream;
    /// use ground::Async;
    ///
    /// # let _ = executor::run(|_| async {
    /// let path = "/tmp/ground-demo";
    /// let stream = Async::<UnixStream>::connect(path).await?;
    /// # Ok::<(), std::io::Error>(())
    /// # });
    /// ```
    ///
    /// [`Unix::connect()`]: https://doc.rust-lang.org/std/os/unix/net/struct.UnixStream.html#method.connect
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self> {
        // Create the socket.
        let type_ = socket2::Type::STREAM.nonblocking().cloexec();
        let socket = socket2::Socket::new(socket2::Domain::UNIX, type_, None)?;
        let mut socket = Async::new(socket);
        // SAFETY: This is just pinning on the stack.
        let mut socket = unsafe { Pin::new_unchecked(&mut socket) };

        // Actually connect.
        let addr = SockAddr::unix(path)?;
        socket
            .as_mut()
            .nonblocking(Interest::Write, |socket| socket.connect(&addr))
            .await?;
        let stream = UnixStream::from(socket.take_inner());
        Ok(Async::new(stream))
    }
}
