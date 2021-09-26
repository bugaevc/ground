//! Asynchronous input/output.

use std::{
    fmt::{self, Debug, Formatter},
    future::Future,
    io::{self, BufRead, IoSlice, IoSliceMut, Read, Result, Write},
    marker::{PhantomData, PhantomPinned},
    os::unix::io::{AsRawFd, RawFd},
    pin::Pin,
    task::{Context, Poll},
};

use futures_io::{AsyncBufRead, AsyncRead, AsyncWrite};

use crate::{reactor::*, util::*};

/// The kind of I/O readiness notifications.
///
/// A value of this type describes what kind of I/O readiness notifications
/// (such as "you can now read form this socket") you're interested in.
///
/// You would normally pass a value of this type into [`Async::nonblocking()`]
/// or [`Async::poll_nonblocking()`] methods, describing the kind if I/O the
/// operation tries to perform, and therefore, what kind of notifications it
/// would be interested in, should the operation fail with [`WouldBlock`].
///
/// [`Async::nonblocking()`]: struct.Async.html#method.nonblocking
/// [`Async::poll_nonblocking()`]: struct.Async.html#method.poll_nonblocking
/// [`WouldBlock`]: https://doc.rust-lang.org/std/io/enum.ErrorKind.html#variant.WouldBlock
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Interest {
    /// Interested in this file becoming readable.
    Read = 1,
    /// Interested in this file becoming writable.
    Write = 2,
    /// Interested in this file becoming readable or writable.
    ReadWrite = 3,
}

impl Interest {
    fn as_events(self) -> u32 {
        match self {
            Interest::Read => libc::EPOLLIN as u32,
            Interest::Write => libc::EPOLLOUT as u32,
            Interest::ReadWrite => (libc::EPOLLIN | libc::EPOLLOUT) as u32,
        }
    }
}

/// A wrapper type to perform I/O asynchronously.
///
/// `Async<T>` wraps a value of type `T`, which must implement the [`AsRawFd`]
/// trait. `Async<T>` automatically puts the wrapped file descriptor into
/// non-blocking mode and registers it with a [`Reactor`] as needed.
///
/// The most straightforward way to use an `Async<T>` is with the standard async
/// I/O traits:
///
/// * If the wrapped type implements [`Read`], `Async<T>` implements [`AsyncRead`].
/// * If the wrapped type implements [`Write`], `Async<T>` implements [`AsyncWrite`].
/// * If the wrapped type implements [`BufRead`], `Async<T>` implements [`AsyncBufRead`].
///
/// [`AsRawFd`]: https://doc.rust-lang.org/std/os/unix/io/trait.AsRawFd.html
/// [`Reactor`]: ../reactor/struct.Reactor.html
/// [`Read`]: https://doc.rust-lang.org/std/io/trait.Read.html
/// [`Write`]: https://doc.rust-lang.org/std/io/trait.Write.html
/// [`BufRead`]: https://doc.rust-lang.org/std/io/trait.BufRead.html
/// [`AsyncRead`]: https://docs.rs/futures/0.3/futures/io/trait.AsyncRead.html
/// [`AsyncWrite`]: https://docs.rs/futures/0.3/futures/io/trait.AsyncWrite.html
/// [`AsyncBufRead`]: https://docs.rs/futures/0.3/futures/io/trait.AsyncBufRead.html
pub struct Async<T: AsRawFd> {
    inner: Option<T>,
    waiter: Waiter,
    _marker: PhantomData<*const ()>,
    _pinned: PhantomPinned,
}

/// Pin projection of `Async`.
///
/// Both fields are not pinned; but `waiter` must always point to a valid
/// waiter.
struct Project<'a, T> {
    inner: Option<&'a mut T>,
    waiter: &'a mut Waiter,
}

impl<T: AsRawFd> Async<T> {
    /// Projects through a pin.
    fn project(self: Pin<&mut Self>) -> Project<'_, T> {
        // SAFETY: This is pin projection. We're not going to be moving anything
        // out.
        let Async {
            ref mut inner,
            ref mut waiter,
            ..
        } = unsafe { self.get_unchecked_mut() };
        Project {
            inner: inner.as_mut(),
            waiter,
        }
    }

    fn ensure_nonblocking(fd: RawFd) -> Result<()> {
        // SAFETY: These are just libc functions that don't access memory.
        let flags = cvt(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
        if flags & libc::O_NONBLOCK == 0 {
            cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) })?;
        }
        Ok(())
    }

    /// Wraps some sort of a file into an `Async<T>`.
    ///
    /// This will automatically put the wrapped file descriptor into
    /// non-blocking mode.
    ///
    /// # Panics
    ///
    /// Panics if putting the file descriptor into non-blocking mode
    /// fails. This happens when the value returned by `as_raw_fd()`
    /// is not, in fact, a valid file descriptor.
    ///
    /// # Examples
    ///
    /// The following example demonstrates wrapping a pre-existing TCP
    /// stream into `Async`.
    ///
    /// ```
    /// use std::net::TcpStream;
    /// use futures::{pin_mut, io::AsyncWriteExt};
    /// use ground::Async;
    ///
    /// # fn preexisting_stream() -> TcpStream {
    /// #     TcpStream::connect("example.com:80").unwrap()
    /// # }
    /// # executor::run(|_| async {
    /// let stream: TcpStream = preexisting_stream();
    /// let async_stream: Async<TcpStream> = Async::new(stream);
    ///
    /// // Now let's write something into the stream, asynchronously.
    /// pin_mut!(async_stream);
    /// async_stream.write_all(b"Hello!\n").await?;
    /// # Ok::<(), std::io::Error>(())
    /// # }).unwrap();
    /// ```
    pub fn new(inner: T) -> Async<T> {
        Self::ensure_nonblocking(inner.as_raw_fd())
            .expect("Failed to put the file descriptor into non-blocking mode");
        let waiter = Waiter {
            waker: noop_waker(),
            register_for: 0,
            waiting_for: 0,
        };
        Async {
            inner: Some(inner),
            waiter,
            _marker: PhantomData,
            _pinned: PhantomPinned,
        }
    }

    /// Ensures we are registered for the given interest.
    ///
    /// This will register the `fd` with the reactor if needed, and update
    /// `interest`.
    fn ensure_registered(fd: RawFd, waiter: &mut Waiter, interest: Interest) {
        let already_registered = waiter.register_for;
        let register_for = already_registered | interest.as_events() | libc::EPOLLET as u32;
        if register_for == already_registered {
            // Nothing to do.
            return;
        }
        waiter.register_for = register_for;

        // SAFETY: We must not move or invalidate the waiter until we remove it
        // from the reactor. We are pinned, so whoever owns us will not move us
        // until we are dropped, at which point we will deregister ourselves
        // from the reactor.
        REACTOR
            .with(|r| unsafe {
                let r = r.borrow();
                if already_registered == 0 {
                    r.add(fd, waiter)
                } else {
                    r.modify(fd, waiter)
                }
            })
            .expect("Failed to register with epoll");
    }

    /// Deregisters ourselves from the reactor, if needed.
    fn deregister(&mut self) {
        if self.waiter.register_for == 0 {
            return;
        }
        let fd = match &self.inner {
            Some(inner) => inner.as_raw_fd(),
            None => return,
        };
        REACTOR
            .with(|r| r.borrow().delete(fd))
            .expect("Failed to deregister from epoll");
        self.waiter.register_for = 0;
    }

    /// Gets a reference to the wrapped value.
    ///
    /// # Examples
    ///
    /// Calling [`TcpStream::local_addr()`] on a `Async::<TcpStream>`:
    ///
    /// ```
    /// use std::net::TcpStream;
    /// # use std::net::ToSocketAddrs;
    /// use ground::Async;
    ///
    /// # executor::run(|_| async {
    /// # let addr = "example.com:80".to_socket_addrs()?.next().unwrap();
    /// let stream = Async::<TcpStream>::connect(addr).await?;
    /// let local_port = stream.get_ref().local_addr()?.port();
    /// # Ok::<(), std::io::Error>(())
    /// # }).unwrap();
    /// ```
    ///
    /// [`TcpStream::local_addr()`]: https://doc.rust-lang.org/std/net/struct.TcpStream.html#method.local_addr
    pub fn get_ref(&self) -> &T {
        self.inner.as_ref().expect("Inner already taken")
    }

    /// Gets a mutable reference to the wrapped value.
    ///
    /// Note that this method takes a `&mut self`, which you likely won't be
    /// able to provide since using an `Async<T>` to perform I/O basically
    /// requires you to pin it. You can use [`pin_get_mut()`] instead.
    ///
    /// [`pin_get_mut()`]: struct.Async.html#method.pin_get_mut
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.as_mut().expect("Inner already taken")
    }

    /// Gets a mutable reference to the wrapped value after you have pinned this
    /// `Async<T>`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::net::TcpStream;
    /// # use std::net::ToSocketAddrs;
    /// use futures::pin_mut;
    /// use ground::Async;
    ///
    /// # executor::run(|_| async {
    /// # let addr = "example.com:80".to_socket_addrs()?.next().unwrap();
    /// let stream = Async::<TcpStream>::connect(addr).await?;
    /// pin_mut!(stream);
    /// // We can still get a `&mut TcpStream` out of it.
    /// let inner: &mut TcpStream = stream.pin_get_mut();
    /// # Ok::<(), std::io::Error>(())
    /// # }).unwrap();
    /// ```
    pub fn pin_get_mut(self: Pin<&mut Self>) -> &mut T {
        self.project().inner.expect("Inner already taken")
    }

    /// Extracts the inner value from this `Async<T>`.
    ///
    /// This will **not** put the inner value into blocking mode.
    ///
    /// Note that this method takes ownership of the `Async<T>`. If you have
    /// already pinned this `Async<T>` and therefore cannot move it, you can use
    /// [`take_inner()`] instead.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::net::TcpStream;
    /// # use std::net::ToSocketAddrs;
    /// use futures::pin_mut;
    /// use ground::Async;
    ///
    /// # executor::run(|_| async {
    /// # let addr = "example.com:80".to_socket_addrs()?.next().unwrap();
    /// let stream = Async::<TcpStream>::connect(addr).await?;
    /// let stream: TcpStream = stream.into_inner();
    /// // Set it back to blocking explicitly, if we want to:
    /// stream.set_nonblocking(false)?;
    /// # Ok::<(), std::io::Error>(())
    /// # }).unwrap();
    /// ```
    ///
    /// [`Reactor`]: ../reactor/struct.Reactor.html
    /// [`take_inner()`]: struct.Async.html#method.take_inner
    pub fn into_inner(mut self) -> T {
        debug_assert_eq!(self.waiter.register_for, 0);
        debug_assert_eq!(self.waiter.waiting_for, 0);
        self.inner.take().expect("Inner already taken")
    }

    /// Extracts the inner value from this `Async<T>` after you have pinned it.
    ///
    /// The only thing you can do with this `Async<T>` after using this method
    /// is drop it. All other operations will panic.
    pub fn take_inner(self: Pin<&mut Self>) -> T {
        // SAFETY: This does not move the waiter out. Moreover, it will call
        // `deregister()`, and after that we don't care about the waiter not
        // moving anymore.
        let this = unsafe { self.get_unchecked_mut() };
        this.deregister();
        this.inner.take().expect("Inner already taken")
    }

    /// Wraps a potentially blocking operation into a poll-style non-blocking
    /// call.
    ///
    /// This is the lowest-level API on top of which other APIs, such as
    /// [`nonblocking()`], [`AsyncRead`], and [`AsyncWrite`], are implemented.
    /// You can use it to implement other non-blocking APIs that make sense for
    /// your type. If you're not wrapping a blocking API into a higher-level
    /// non-blocking API, you should not need to use this method directly. In
    /// many cases, it is more convenient to use [`nonblocking()`] instead of
    /// this method.
    ///
    /// **Poll-style non-blocking APIs** typically take form of methods that
    /// accept `Pin<&mut Self>`, a task context, and any other API-specific
    /// arguments, and whose return type is [`Poll`]. Such a method is expected
    /// to either perform the operation and return its result wrapped in
    /// [`Poll::Ready`], or determine that the operation can not be completed
    /// yet, register the current task to be woken up later when it might make
    /// progress, and return [`Poll::Pending`].
    ///
    /// **Low-level OS calls** typically block the calling thread until the
    /// requested operation is completed. When used on a file descriptor that
    /// has been put into non-blocking mode, they instead return an I/O error of
    /// kind [`ErrorKind::WouldBlock`] if the operation cannot be completed
    /// immediately, without blocking.
    ///
    /// `Async::poll_nonblocking()` wraps the latter behavior into the former
    /// interface. It will invoke the given closure, passing it a mutable
    /// reference to the wrapped value as an argument. The closure should
    /// attempt to perform the blocking operation, and must return an
    /// [`std::io::Result`].
    ///
    /// * If the closure returns an error of kind [`ErrorKind::WouldBlock`],
    ///   this method will ensure that the current task will get woken up
    ///   according to the passed in `interest`. This method then returns
    ///   [`Poll::Pending`]. When the task gets woken up, it is up to you to
    ///   retry the operation by calling this method again, if you're still
    ///   interested in its result.
    ///
    /// * If the closure returns anything else (whether success or an error),
    ///   this method wraps the return value into [`Poll::Ready`] and returns it
    ///   immediately.
    ///
    /// As a special case, if the last time this method was called with the same
    /// `interest`, the closure has returned an error of kind
    /// [`ErrorKind::WouldBlock`], and the reactor has not yet indicated that
    /// the file descriptor is ready to make progress according to this
    /// `interest`, this method will skip invoking the closure, and will return
    /// [`Poll::Pending`] straight away.
    ///
    /// # Arguments
    ///
    /// * `cx` - The current task context.
    /// * `interest` - The kind of I/O readiness notifications this operation needs.
    /// * `f` - A closure that attempts a potentially blocking operation.
    ///
    /// # Examples
    ///
    /// Imagine we have a trait for some blocking operation:
    ///
    /// ```
    /// trait Foo {
    ///    fn foo(&mut self, arg: i32) -> std::io::Result<usize>;
    /// }
    /// ```
    ///
    /// And a corresponding non-blocking trait:
    ///
    /// ```
    /// # use std::{pin::Pin, task::{Context, Poll}};
    /// trait AsyncFoo {
    ///     fn poll_foo(
    ///         self: Pin<&mut Self>,
    ///         cx: &mut Context<'_>,
    ///         arg: i32
    ///     ) -> Poll<std::io::Result<usize>>;
    /// }
    /// ```
    ///
    /// We could then implement `AsyncFoo` for `Async<T>` where `T: Foo` like
    /// this (assuming *foo* is a read-like operation):
    ///
    /// ```
    /// # use std::{io, pin::Pin, task::{Context, Poll}, os::unix::io::AsRawFd};
    /// # use ground::io::{Async, Interest};
    /// #
    /// # trait Foo {
    /// #    fn foo(&mut self, arg: i32) -> std::io::Result<usize>;
    /// # }
    /// #
    /// # trait AsyncFoo {
    /// #     fn poll_foo(
    /// #         self: Pin<&mut Self>,
    /// #         cx: &mut Context<'_>,
    /// #         arg: i32
    /// #     ) -> Poll<std::io::Result<usize>>;
    /// # }
    /// #
    /// impl<T: Foo + AsRawFd> AsyncFoo for Async<T> {
    ///    fn poll_foo(
    ///        self: Pin<&mut Self>,
    ///        cx: &mut Context<'_>,
    ///        arg: i32
    ///    ) -> Poll<io::Result<usize>> {
    ///        self.poll_nonblocking(cx, Interest::Read, |inner| inner.foo(arg))
    ///    }
    /// }
    /// ```
    ///
    /// [`ErrorKind::WouldBlock`]: https://doc.rust-lang.org/std/io/enum.ErrorKind.html#variant.WouldBlock
    /// [`Poll`]: https://doc.rust-lang.org/std/task/enum.Poll.html
    /// [`Poll::Pending`]: https://doc.rust-lang.org/std/task/enum.Poll.html#variant.Pending
    /// [`Poll::Ready`]: https://doc.rust-lang.org/std/task/enum.Poll.html#variant.Ready
    /// [`nonblocking()`]: struct.Async.html#method.nonblocking
    /// [`AsyncRead`]: struct.Async.html#impl-AsyncRead
    /// [`AsyncWrite`]: struct.Async.html#impl-AsyncWrite
    /// [`std::io::Result`]: https://doc.rust-lang.org/std/io/type.Result.html
    pub fn poll_nonblocking<'a, R>(
        self: Pin<&'a mut Self>,
        cx: &mut Context<'_>,
        interest: Interest,
        f: impl FnOnce(&'a mut T) -> Result<R>,
    ) -> Poll<Result<R>> {
        let this = self.project();
        let inner = this.inner.expect("Inner already taken");
        if interest.as_events() & this.waiter.waiting_for != 0 {
            // We've been blocked on this since last time,
            // and still haven't received a wake-up from reactor.
            // Bail out immediately.
            return Poll::Pending;
        }
        let fd = inner.as_raw_fd();
        match f(inner) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                this.waiter.waker = cx.waker().clone();
                this.waiter.waiting_for |= interest.as_events();
                Self::ensure_registered(fd, this.waiter, interest);
                Poll::Pending
            }
            anything_else => Poll::Ready(anything_else),
        }
    }

    /// Wraps a potentially blocking operation into a non-blocking [`Future`].
    ///
    /// Low-level OS calls typically block the calling thread until the
    /// requested operation is completed. When used on a file descriptor that
    /// has been put into non-blocking mode, they instead return an I/O error of
    /// kind [`ErrorKind::WouldBlock`] if the operation cannot be completed
    /// immediately, without blocking.
    ///
    /// `Async::nonblocking()` wraps this behavior into a [`Future`] that you
    /// can then conveniently `.await`. It will invoke the given closure,
    /// passing it a mutable reference to the wrapped value as an argument. The
    /// closure should attempt to perform the blocking operation, and must
    /// return an [`std::io::Result`]. The closure will be *repeatedly* invoked
    /// until it returns something other than an error of kind
    /// [`ErrorKind::WouldBlock`].
    ///
    /// # Examples
    ///
    /// Here's how you can call [`UdpSocket::recv_from()`] without blocking:
    ///
    /// ```
    /// use std::net::UdpSocket;
    /// use futures::pin_mut;
    /// use ground::io::{Async, Interest};
    ///
    /// # executor::run(|_| async {
    /// # let socket = UdpSocket::bind("127.0.0.1:8000")?;
    /// # socket.send_to(b"Hello\n", socket.local_addr()?)?;
    /// let socket: Async<UdpSocket> = Async::new(socket);
    /// pin_mut!(socket);
    ///
    /// let mut buf = [0; 256];
    /// let (size, addr) = socket.nonblocking(
    ///     Interest::Read,
    ///     |inner| inner.recv_from(&mut buf)
    /// ).await?;
    /// # Ok::<(), std::io::Error>(())
    /// # }).unwrap();
    /// ```
    ///
    /// [`Future`]: https://doc.rust-lang.org/std/future/trait.Future.html
    /// [`std::io::Result`]: https://doc.rust-lang.org/std/io/type.Result.html
    /// [`ErrorKind::WouldBlock`]: https://doc.rust-lang.org/std/io/enum.ErrorKind.html#variant.WouldBlock
    /// [`UdpSocket::recv_from()`]: https://doc.rust-lang.org/std/net/struct.UdpSocket.html#method.recv_from
    pub fn nonblocking<F, R>(
        self: Pin<&mut Self>,
        interest: Interest,
        f: F,
    ) -> Nonblocking<'_, T, F>
    where
        F: FnMut(&mut T) -> Result<R>,
    {
        Nonblocking {
            a: self,
            interest,
            f,
        }
    }
}

impl<T: AsRawFd> AsRawFd for Async<T> {
    fn as_raw_fd(&self) -> RawFd {
        match &self.inner {
            Some(inner) => inner.as_raw_fd(),
            None => panic!("Inner already taken"),
        }
    }
}

impl<T: AsRawFd> Drop for Async<T> {
    fn drop(&mut self) {
        self.deregister();
    }
}

impl<T: AsRawFd + Debug> Debug for Async<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Async").field(&self.inner).finish()
    }
}

/// The future type returned from [`Async::nonblocking()`].
///
/// [`Async::nonblocking()`]: struct.Async.html#method.nonblocking
pub struct Nonblocking<'a, T: AsRawFd, F> {
    a: Pin<&'a mut Async<T>>,
    interest: Interest,
    f: F,
}

impl<'a, T: AsRawFd, F> Unpin for Nonblocking<'a, T, F> {}

impl<'a, T, R, F> Future for Nonblocking<'a, T, F>
where
    T: AsRawFd,
    F: FnMut(&mut T) -> Result<R>,
{
    type Output = Result<R>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Nonblocking {
            ref mut a,
            ref mut f,
            interest,
        } = *self;
        a.as_mut().poll_nonblocking(cx, interest, f)
    }
}

/// When the wrapped type implements [`Read`], `Async<T>` implements [`AsyncRead`].
///
/// [`Read`]: https://doc.rust-lang.org/std/io/trait.Read.html
/// [`AsyncRead`]: https://docs.rs/futures/0.3/futures/io/trait.AsyncRead.html
impl<T: Read + AsRawFd> AsyncRead for Async<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize>> {
        self.poll_nonblocking(cx, Interest::Read, |inner| inner.read(buf))
    }

    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<Result<usize>> {
        self.poll_nonblocking(cx, Interest::Read, |inner| inner.read_vectored(bufs))
    }
}

/// When the wrapped type implements [`Write`], `Async<T>` implements [`AsyncWrite`].
///
/// [`Write`]: https://doc.rust-lang.org/std/io/trait.Write.html
/// [`AsyncWrite`]: https://docs.rs/futures/0.3/futures/io/trait.AsyncWrite.html
impl<T: Write + AsRawFd> AsyncWrite for Async<T> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize>> {
        self.poll_nonblocking(cx, Interest::Write, |inner| inner.write(buf))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize>> {
        self.poll_nonblocking(cx, Interest::Write, |inner| inner.write_vectored(bufs))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.poll_nonblocking(cx, Interest::Write, Write::flush)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        match self
            .as_mut()
            .poll_nonblocking(cx, Interest::Write, Write::flush)
        {
            Poll::Ready(Ok(())) => {
                self.take_inner();
                Poll::Ready(Ok(()))
            }
            anything_else => anything_else,
        }
    }
}

/// When the wrapped type implements [`BufRead`], `Async<T>` implements [`AsyncBufRead`].
///
/// [`BufRead`]: https://doc.rust-lang.org/std/io/trait.BufRead.html
/// [`AsyncBufRead`]: https://docs.rs/futures/0.3/futures/io/trait.AsyncBufRead.html
impl<T: BufRead + AsRawFd> AsyncBufRead for Async<T> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<&[u8]>> {
        self.poll_nonblocking(cx, Interest::Read, BufRead::fill_buf)
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        self.pin_get_mut().consume(amt)
    }
}
