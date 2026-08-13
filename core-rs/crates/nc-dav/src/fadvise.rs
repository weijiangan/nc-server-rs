//! `posix_fadvise` cache hints (phase-23.6).
//!
//! The only `unsafe` in this crate, deliberately contained here.  rustix's
//! `fs::fadvise` is unusable from outside rustix — its `Advice` parameter
//! type lives in a private module (still true on rustix main) — and a
//! third-party wrapper would hide the same `unsafe` inside a barely-maintained
//! dependency.  So this module carries a 10-line shim instead, with the crate
//! root relaxed from `forbid(unsafe_code)` to `deny(unsafe_code)` so this
//! block stays the *only* one.
//!
//! # Safety rationale for the `unsafe` block in [`hint`]
//!
//! `posix_fadvise` takes no pointers and cannot corrupt memory: it is a
//! kernel hint, and its only failure mode is a negative return value (a hint
//! the kernel ignores).  The fd is a live `File` borrow, and the advice
//! constants are compile-time values.  All hints are best-effort: a failed
//! hint has no user-visible effect, so failures are logged at `debug!` and
//! ignored (the kernel may reject the advice on some filesystems — e.g.
//! `EINVAL` on tmpfs).

use std::fs::File;
use std::os::unix::io::AsRawFd;

/// `posix_fadvise` advice values, named like the kernel constants.
#[derive(Debug, Clone, Copy)]
#[repr(i32)]
pub(crate) enum Advice {
    WillNeed = libc::POSIX_FADV_WILLNEED,
    Sequential = libc::POSIX_FADV_SEQUENTIAL,
    DontNeed = libc::POSIX_FADV_DONTNEED,
}

/// Issue a `posix_fadvise` hint covering the whole file.
#[allow(unsafe_code)] // the one documented unsafe in this crate — see module docs
pub(crate) fn hint(file: &File, advice: Advice) {
    // `0, 0` = whole file (offset 0, len 0 = to EOF).
    let ret = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, advice as libc::c_int) };
    if ret != 0 {
        tracing::debug!(?advice, errno = ret, "posix_fadvise failed");
    }
}
