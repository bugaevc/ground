//! Waiting for I/O events.

use std::{
    cell::RefCell,
    io::Result,
    marker::PhantomData,
    mem::MaybeUninit,
    os::unix::io::{AsRawFd, RawFd},
    ptr,
    task::Waker,
};

use crate::util::cvt;

/// Something waiting for events.
pub(crate) struct Waiter {
    /// The waker to wake when the events arrive.
    pub(crate) waker: Waker,
    /// What events we are registered for (or want to register for). This is
    /// currently the raw epoll flags.
    pub(crate) register_for: u32,
    /// What events we are actually waiting for this time.
    ///
    /// We're using edge-triggered notifications, so we won't receive
    /// notifications about some kinds of I/O even if we're registered for those
    /// I/O kinds. Say, if a socket becomes writable, but we're not interested
    /// in writing, we won't receive writability notifications after receiving
    /// it once. We may be waiting to read from this socket instead. So this
    /// field stores what kind of I/O we're actually interested in, or in other
    /// words what we're currently waiting for.
    pub(crate) waiting_for: u32,
}

/// A reactor to receive I/O readiness notifications.
#[derive(Debug)]
pub struct Reactor {
    /// The underlying epoll filed descriptor.
    epfd: RawFd,
    /// Whether we're currently dispatching received events.
    is_dispatching: bool,
    /// The reactor is `!Sync + !Send`.
    _marker: PhantomData<*mut Waiter>,
}

impl Reactor {
    /// Creates a new reactor.
    ///
    /// It's unlikely that you actually want to call this; use the [`REACTOR`]
    /// thread-local instead.
    ///
    /// [`REACTOR`]: constant.REACTOR.html
    pub fn new() -> Reactor {
        // SAFETY: This is just an external function that doesn't access any pointers.
        let epfd = cvt(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })
            .expect("epoll_create1() failed");

        Reactor {
            epfd,
            is_dispatching: false,
            _marker: PhantomData,
        }
    }

    /// Adds the given fd/waiter pair to the reactor.
    ///
    /// When the fd receives a readiness notification (during a [`dispatch()`]
    /// call), the waiter will be woken up. Thus, the waiter must not be moved
    /// or invalidated until it is [deleted] form the reactor.
    ///
    /// [`dispatch()`]: #method.dispatch
    /// [deleted]: #method.delete
    pub(crate) unsafe fn add(&self, fd: RawFd, waiter: *mut Waiter) -> Result<()> {
        let mut event = libc::epoll_event {
            events: (*waiter).register_for,
            u64: waiter as u64,
        };
        cvt(libc::epoll_ctl(
            self.epfd,
            libc::EPOLL_CTL_ADD,
            fd,
            &mut event,
        ))
        .map(drop)
    }

    /// Modifies the events an fd/waiter pair is registered for.
    ///
    /// This must be an already registered fd/waiter pair.
    pub(crate) unsafe fn modify(&self, fd: RawFd, waiter: *mut Waiter) -> Result<()> {
        let mut event = libc::epoll_event {
            events: (*waiter).register_for,
            u64: waiter as u64,
        };
        cvt(libc::epoll_ctl(
            self.epfd,
            libc::EPOLL_CTL_MOD,
            fd,
            &mut event,
        ))
        .map(drop)
    }

    /// Deletes an fd/waiter pair from the reactor.
    ///
    /// After this method successfully returns, the waiter can be safely moved
    /// and/or invalidated.
    ///
    /// # Panics
    ///
    /// This method panics if called during I/O dispatch. This is because in
    /// that case, there could still be further events queued for this fd/waiter
    /// pair. It is expected that this method will only be called after the
    /// reactor is done dispatching, in the task running phase.
    ///
    /// It is this guarantee that allows the caller to destroy/invalidate the
    /// waiter immediately after calling this method.
    pub(crate) fn delete(&self, fd: RawFd) -> Result<()> {
        assert!(
            !self.is_dispatching,
            "Can't remove I/O sources during dispatch"
        );
        // SAFETY: This is an extern function, and with `EPOLL_CTL_DEL` it does
        // not try to access the events pointer, so we pass `null_mut()`.
        cvt(unsafe { libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_DEL, fd, ptr::null_mut()) })
            .map(drop)
    }

    /// Deliver this event to the waiter.
    ///
    /// # Safety
    ///
    /// The data inside of the event must point to a valid waiter.
    unsafe fn deliver(event: libc::epoll_event) {
        let waiter = event.u64 as *mut Waiter;
        {
            // We've received this I/O, so we're no longer waiting for it.
            // Let the waiter know.
            let waiting_for: &mut u32 = &mut (*waiter).waiting_for;
            *waiting_for &= !event.events;
        }
        {
            // And now, wake the waker.
            let waker: &Waker = &(*waiter).waker;
            waker.wake_by_ref();
        }
    }

    /// Waits for events and dispatches them.
    ///
    /// If you already know that there are event ready to be read and
    /// dispatched, this will not block; otherwise it might.
    ///
    /// This will only wake the tasks that wait for events; you need an executor
    /// to let the tasks actually make progress.
    pub fn dispatch(&mut self) -> Result<()> {
        assert!(!self.is_dispatching, "Recursive dispatch?");

        // SAFETY: A `MaybeUninit` itself can always be considered initialized.
        // See [this example] in `MaybeUninit` docs for a longer explanation of
        // why this is fine.
        //
        // [this example]: https://doc.rust-lang.org/std/mem/union.MaybeUninit.html#initializing-an-array-element-by-element
        let mut buffer: [MaybeUninit<libc::epoll_event>; 512] =
            unsafe { MaybeUninit::uninit().assume_init() };
        // SAFETY: This will write into the provided buffer, and we're passing a valid buffer pointer.
        // It will not read from it, so it's fine that we're not initializing it.
        let num_events = cvt(unsafe {
            libc::epoll_wait(
                self.epfd,
                buffer.as_mut_ptr() as *mut libc::epoll_event,
                buffer.len() as i32,
                -1,
            )
        })? as usize;

        // Now, do dispatch.
        self.is_dispatching = true;
        for event in &buffer[..num_events] {
            // SAFETY: `epoll_wait()` told us it has initialized `num_events`
            // events in the array, and we're not reading past that.
            let event: libc::epoll_event = unsafe { *event.as_ptr() };
            // SAFETY: We're only receiving events we have registered for. We
            // have only registered for events with valid data pointers.
            unsafe { Self::deliver(event) };
        }
        self.is_dispatching = false;
        Ok(())
    }
}

/// Get the file descriptor backing this reactor.
///
/// You can use it to wait for events to arrive yourself, instead of letting
/// [`dispatch()`] do it. Once you know there are events queued, you can call
/// [`dispatch()`] and be sure it doesn't block. This can be used to integrate
/// `ground` into another reactor.
///
/// [`dispatch()`]: #method.dispatch
impl AsRawFd for Reactor {
    fn as_raw_fd(&self) -> RawFd {
        self.epfd
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        cvt(unsafe { libc::close(self.epfd) }).expect("failed to close epoll fd");
    }
}

thread_local! {
    /// Lazily created, per-thread reactor instance.
    pub static REACTOR: RefCell<Reactor> = RefCell::new(Reactor::new());
}
