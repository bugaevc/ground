//! A lightweight `epoll` reactor.
//!
//! This crate implements a simple lightweight reactor as a thin wrapper over `epoll`.
//!
//! Design goals:
//! * No allocations (not a single one!);
//! * No locking/synchronization;
//! * Minimal dependencies;
//! * Be a simple, thin wrapper over `epoll`.
//!
//! There are no `Box`es, `Arc`s, `Mutex`es, `Vec`s, `HashMap`s, or any other
//! collections or synchronization primitives in *ground*.
//!
//! There are downsides to this design, which can be expressed as it being
//! `!Send + !Sync + !Unpin`. We'll see what exactly this means in a minute.
//!
//! The primary API exposed by this crate is the [`Async<T>`] wrapper type. It
//! can wrap normal, blocking I/O types and be used to perform non-blocking I/O
//! on them.
//!
//! # Creating an `Async<T>`
//!
//! If you have a pre-existing instance of a blocking I/O type `T`, you can wrap
//! it into [`Async<T>`] by simply calling [`Async::new()`]. You'd typically
//! create an instance of the a blocking I/O type using some type-specific
//! method, and then immediately wrap it into an `Async<>`.
//!
//! The following example shows how you could spawn a child process, connecting
//! its standard output to a pipe, and wrap that pipe into `Async<>`:
//!
//! ```
//! use std::process::{Command, Stdio, ChildStdout};
//! use ground::Async;
//!
//! # fn main() -> std::io::Result<()> {
//! // Spawn a child process.
//! let child = Command::new("/bin/uname")
//!     .stdout(Stdio::piped())
//!     .spawn()?;
//! let stdout: ChildStdout = child.stdout.unwrap();
//! // Wrap its stdout pipe into `Async<>`.
//! let async_stdout: Async<ChildStdout> = Async::new(stdout);
//! # Ok(())
//! # }
//! ```
//!
//! You can also encounter types whose construction itself is blocking. For
//! example, to create a connected TCP socket, you'd normally use
//! [`TcpStream::connect()`], which is an associated function on [`TcpStream`]
//! that blocks until a connection is made, and then returns an already
//! connected [`TcpStream`]. Since you get a [`TcpStream`] only once the
//! connection is established, there's nothing you could wrap into `Async<>` to
//! make connecting non-blocking.
//!
//! To deal with this, you could use lower-level API, such as using `socket(2)`
//! and `connect(2)` syscalls instead of [`TcpStream::connect()`]. For your
//! convenience, an implementation of this is included in *ground* as
//! [`Async::<TcpStream>::connect()`].
//!
//! # Reading and writing
//!
//! The most straightforward way to use an `Async<T>` is with the standard async
//! I/O traits:
//!
//! * If the wrapped type implements [`Read`], `Async<T>` implements [`AsyncRead`].
//! * If the wrapped type implements [`Write`], `Async<T>` implements [`AsyncWrite`].
//! * If the wrapped type implements [`BufRead`], `Async<T>` implements [`AsyncBufRead`].
//!
//! For example, since [`TcpStream`] implements [`Read`] and [`Write`], we can
//! use [`AsyncRead`] and [`AsyncWrite`] with `Async<TcpStream>`:
//!
//! ```
//! use std::net::TcpStream;
//! use futures::{pin_mut, io::{AsyncReadExt, AsyncWriteExt}};
//! use ground::Async;
//!
//! async fn do_some_io(stream: Async<TcpStream>) -> std::io::Result<()> {
//!     pin_mut!(stream);
//!
//!     stream.write_all(b"Hello!\n").await?;
//!
//!     let mut buffer = [0u8; 4];
//!     stream.read_exact(&mut buffer).await?;
//!     dbg!(buffer);
//!
//!     Ok(())
//! }
//! ```
//!
//! You'll notice we've had to pin the stream. The reason for this is two-fold.
//!
//! First, `Async<>` does not implement `Unpin`, and needs to be pinned for you
//! to perform basically any kind of I/O on it. This is because *ground* passes
//! a direct pointer into an `Async<>` to the kernel as a part of `epoll` data
//! (this pointer is later used to notify the task it can make progress).
//! Therefore, you just can't move an `Async<>` once you start performing any
//! I/O on it.
//!
//! Second, the [`AsyncReadExt::read_exact()`] and
//! [`AsyncWriteExt::write_all()`] convenience methods that we're using here (as
//! well as many other similar methods) require the value the method is called
//! on to implement `Unpin` — which `Async<>`, of course, does not. But
//! `Pin<&mut T>`, being just a reference, *does* implement `Unpin` even if `T:
//! !Unpin`, and furthermore implementations of [`AsyncRead`]/[`AsyncWrite`] can
//! be
//! [forwarded](https://docs.rs/futures/0.3/futures/io/trait.AsyncRead.html#impl-AsyncRead-for-Pin%3CP%3E)
//! through a `Pin<&mut T>`. So in the example above, we're not actually not
//! using `<Async<TcpStream> as AsyncReadExt>`, we're using `<Pin<&mut
//! Async<TcpStream>> as AsyncReadExt>`.
//!
//! You don't have to care for all these details most of the time, but you do
//! have to pin your `Async`s everywhere before performing I/O on them. You can
//! normally use [`futures::pin_mut!()`] or <a
//! href="https://docs.rs/pin-project">`#[pin_project]`</a> macros to perform
//! the pinning safely and conveniently.
//!
//! # Other operations
//!
//! You may want to do more than just read and write bytes. You may want to
//! accept a connection, or receive a datagram, or perform some other,
//! type-specific I/O operation, without blocking.
//!
//! For some of the operations, *ground* provides ready-made async wrappers,
//! such as [`Async::<TcpListener>::accept()`]. However, it would be neither
//! feasible nor desirable for *ground* to provide a wrapper for every single
//! operation you may potentially want to perform. Instead, *ground* provides an
//! API that enables *you* to wrap any potentially blocking operation on a file
//! descriptor into a non-blocking one.
//!
//! The API is the [`Async::nonblocking()`] method (and its lower-level
//! counterpart, [`Async::poll_nonblocking()`]).
//!
//! # Bring your own executor
//!
//! *ground* is an I/O *reactor*; it wakes tasks when they are ready to make
//! progress. For the tasks to execute their logic, you also need an *executor*.
//! There are multiple executor implementations in the Rust ecosystem (including
//! [ones provided by the `futures`
//! crate](https://docs.rs/futures/0.3/futures/executor)), and many platforms
//! provide their own executors as well.
//!
//! While *ground* is not tied to any particular executor, it has some
//! constraints on how you can use it with an executor. Informally, it is `!Send
//! + !Sync`, which means:
//!
//! * `Async<>` does not implement `Send` or `Sync`, and so futures containing
//!   it don't either. This means you cannot use a thread pool executor with
//!   *ground*. That being said, you can still use a multi-threaded executor;
//!   the only constraint is that it must not move futures *between* threads:
//!   each future must stay in the thread it was originally created on.
//!
//! * The executor and the [`Reactor`] have to run in the same thread. In case
//!   of a multi-threaded executor, each thread has its own `Reactor`.
//!
//! There are two major approaches to integrating an executor and the `Reactor`:
//!
//! 1. Make `Reactor` drive the main loop. In this approach,
//!    [`Reactor::dispatch()`] is used to both wait for I/O events and to
//!    dispatch them, and the executor is run each time in between calls to
//!    `Reactor::dispatch()`.
//!
//!    A typical main loop with this approach looks like this:
//!
//!    ```
//!    use futures::executor::LocalPool;
//!
//!    let mut pool = LocalPool::new();
//!    // ...spawn some initial tasks...
//!    loop {
//!    #   break;
//!        // Run the executor to let tasks make progress.
//!        pool.run_until_stalled();
//!        // Wait for I/O events.
//!        ground::reactor::REACTOR.with(|r| r.borrow_mut().dispatch())
//!            .expect("Failed to dispatch the reactor");
//!    }
//!    ```
//!
//!    When not under heavy load, the loop will spend most of the time waiting
//!    for I/O inside `Reactor::dispatch()`. Crucially, the executor must not
//!    block the thread waiting for more tasks to run, otherwise the reactor
//!    would never get a chance to dispatch I/O events.
//!
//! 2. Embed *ground* into a different reactor. This is typically done when
//!    integrating *ground* into a platform-provided event loop. In this case,
//!    the outer reactor drives the main loop and waits for I/O.
//!
//!    Since `Reactor` itself implements `AsRawFd` (exposing its `epoll` file
//!    descriptor), you can ask the outer reactor to watch for it becoming
//!    readable just like you would for any other file descriptor. The outer
//!    reactor will indicate that the file descriptor is readable when there are
//!    pending events for the inner `Reactor`, at which point you should call
//!    `Reactor::dispatch()` to dispatch them.
//!
//!    Depending on the outer event loop API, it might look somewhat like this:
//!
//!    ```
//!    use std::os::unix::io::AsRawFd;
//!    # use std::os::unix::io::RawFd;
//!
//!    let fd = ground::reactor::REACTOR.with(|r| r.borrow().as_raw_fd());
//!    # struct Outer;
//!    # impl Outer {
//!    #     fn on_fd_readable(&mut self, fd: RawFd, f: impl FnMut()) {}
//!    # }
//!    # let mut outer_event_loop = Outer;
//!
//!    outer_event_loop.on_fd_readable(fd, || {
//!        ground::reactor::REACTOR.with(|r| r.borrow_mut().dispatch())
//!            .expect("Failed to dispatch the reactor");
//!    });
//!    ```
//!
//! [`Async<T>`]: struct.Async.html
//! [`Async::new()`]: struct.Async.html#method.new
//!
//! [`TcpStream`]: https://doc.rust-lang.org/std/net/struct.TcpStream.html
//! [`Async::<TcpStream>::connect()`]: struct.Async.html#method.connect
//! [`Async::<TcpListener>::accept()`]: struct.Async.html#method.accept
//! [`TcpStream::connect()`]: https://doc.rust-lang.org/std/net/struct.TcpStream.html#method.connect
//!
//! [`Read`]: https://doc.rust-lang.org/std/io/trait.Read.html
//! [`Write`]: https://doc.rust-lang.org/std/io/trait.Write.html
//! [`BufRead`]: https://doc.rust-lang.org/std/io/trait.BufRead.html
//! [`AsyncRead`]: https://docs.rs/futures/0.3/futures/io/trait.AsyncRead.html
//! [`AsyncWrite`]: https://docs.rs/futures/0.3/futures/io/trait.AsyncWrite.html
//! [`AsyncBufRead`]: https://docs.rs/futures/0.3/futures/io/trait.AsyncBufRead.html
//!
//! [`AsyncReadExt::read_exact()`]: https://docs.rs/futures/0.3/futures/io/trait.AsyncReadExt.html#method.read_exact
//! [`AsyncWriteExt::write_all()`]: https://docs.rs/futures/0.3/futures/io/trait.AsyncWriteExt.html#method.write_all
//! [`futures::pin_mut!()`]: https://docs.rs/futures/0.3/futures/macro.pin_mut.html
//!
//! [`Async::nonblocking()`]: struct.Async.html#method.nonblocking
//! [`Async::poll_nonblocking()`]: struct.Async.html#method.poll_nonblocking
//!
//! [`Reactor::dispatch()`]: struct.Reactor.html#method.dispatch

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod io;
pub mod reactor;
mod util;

#[cfg_attr(docsrs, doc(cfg(feature = "net")))]
#[cfg(feature = "net")]
mod net;

#[cfg_attr(docsrs, doc(cfg(feature = "unix")))]
#[cfg(feature = "unix")]
mod unix;

#[cfg_attr(docsrs, doc(cfg(feature = "dns")))]
#[cfg(feature = "dns")]
pub mod dns;

#[doc(inline)]
pub use crate::io::Async;
#[doc(inline)]
pub use crate::reactor::Reactor;
