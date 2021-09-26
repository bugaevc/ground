use std::{
    io,
    task::{RawWaker, RawWakerVTable, Waker},
};

/// Convert a C-style return code + `errno` into a Rust-style `io::Result`.
pub(crate) fn cvt(return_code: libc::c_int) -> io::Result<libc::c_int> {
    if return_code < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(return_code)
    }
}

/// Make a waker that wakes nothing.
///
/// The `RawWaker` API is quite unwieldy, so this function wraps the gory
/// details.
pub(crate) fn noop_waker() -> Waker {
    const RAW_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    const RAW_WAKER: RawWaker = RawWaker::new(std::ptr::null(), &RAW_WAKER_VTABLE);

    fn clone(_data: *const ()) -> RawWaker {
        RAW_WAKER
    }
    fn wake(_data: *const ()) {}
    fn wake_by_ref(_data: *const ()) {}
    fn drop(_data: *const ()) {}

    // SAFETY: We manage no resources, and we never dereference the pointers we
    // get passed, so everything is safe.
    unsafe { Waker::from_raw(RAW_WAKER) }
}
