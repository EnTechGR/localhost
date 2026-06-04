/// Raw `epoll` wrapper.
///
/// Provides a thin, safe Rust interface over the Linux `epoll_*` syscalls.
/// All syscall invocations are `unsafe`; the public API is safe by
/// construction (invariants enforced at the boundary).
///
/// # Design notes
///
/// - One `Epoll` instance is created at startup and lives for the entire
///   process lifetime.
/// - The `token` stored in `epoll_data` is always the raw file descriptor
///   cast to `u64`. This makes dispatch O(1): look up the fd in the registry
///   without an extra indirection table.
/// - `EPOLLRDHUP` is always added alongside caller-supplied events so the
///   dispatcher can detect half-closed connections without an extra read.
/// - `EPOLLET` (edge-triggered) is intentionally **not** used; level-triggered
///   mode is simpler and our non-blocking reads/writes drain the buffers
///   completely anyway.
use std::os::unix::io::RawFd;

use libc::{
    epoll_create1, epoll_ctl, epoll_event, epoll_wait, EPOLL_CLOEXEC, EPOLL_CTL_ADD,
    EPOLL_CTL_DEL, EPOLL_CTL_MOD,
};

// Re-export the flag constants so callers don't need to import libc directly.
pub use libc::{EPOLLERR, EPOLLHUP, EPOLLIN, EPOLLOUT, EPOLLRDHUP};

// ---------------------------------------------------------------------------
// EpollError
// ---------------------------------------------------------------------------

/// Errors that can arise from epoll operations.
#[derive(Debug)]
pub enum EpollError {
    /// `epoll_create1` failed.
    CreateFailed(i32),
    /// `epoll_ctl` failed (add/modify/delete).
    CtlFailed { op: &'static str, fd: RawFd, errno: i32 },
    /// `epoll_wait` failed with an unexpected errno.
    WaitFailed(i32),
}

impl std::fmt::Display for EpollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EpollError::CreateFailed(e)              => write!(f, "epoll_create1 failed: errno {e}"),
            EpollError::CtlFailed { op, fd, errno }  => write!(f, "epoll_ctl({op}) fd={fd} failed: errno {errno}"),
            EpollError::WaitFailed(e)                => write!(f, "epoll_wait failed: errno {e}"),
        }
    }
}

impl std::error::Error for EpollError {}

// ---------------------------------------------------------------------------
// Epoll
// ---------------------------------------------------------------------------

/// Owning wrapper around a Linux epoll file descriptor.
///
/// Dropped via `Drop` which calls `close(2)` on the epoll fd.
pub struct Epoll {
    fd: RawFd,
}

impl Epoll {
    /// Create a new epoll instance.
    ///
    /// Uses `EPOLL_CLOEXEC` so the fd is not inherited by child processes
    /// (important for CGI forks).
    pub fn create() -> Result<Self, EpollError> {
        let fd = unsafe { epoll_create1(EPOLL_CLOEXEC) };
        if fd < 0 {
            return Err(EpollError::CreateFailed(errno()));
        }
        Ok(Epoll { fd })
    }

    /// Register `fd` with the epoll instance.
    ///
    /// `events`  – bitmask of `EPOLLIN`, `EPOLLOUT`, etc.
    /// `token`   – stored verbatim in `epoll_data.u64`; callers pass `fd as u64`.
    ///
    /// `EPOLLRDHUP` is OR-ed in automatically so half-close is always detected.
    pub fn add(&self, fd: RawFd, events: u32, token: u64) -> Result<(), EpollError> {
        let mut ev = make_event(events | EPOLLRDHUP as u32, token);
        let rc = unsafe { epoll_ctl(self.fd, EPOLL_CTL_ADD, fd, &mut ev) };
        if rc < 0 {
            return Err(EpollError::CtlFailed { op: "ADD", fd, errno: errno() });
        }
        Ok(())
    }

    /// Modify the event mask for a previously registered `fd`.
    ///
    /// Use this to arm `EPOLLOUT` once a response is ready to write, or to
    /// switch back to `EPOLLIN` after a write completes.
    ///
    /// `EPOLLRDHUP` is OR-ed in automatically.
    pub fn modify(&self, fd: RawFd, events: u32, token: u64) -> Result<(), EpollError> {
        let mut ev = make_event(events | EPOLLRDHUP as u32, token);
        let rc = unsafe { epoll_ctl(self.fd, EPOLL_CTL_MOD, fd, &mut ev) };
        if rc < 0 {
            return Err(EpollError::CtlFailed { op: "MOD", fd, errno: errno() });
        }
        Ok(())
    }

    /// Remove `fd` from the epoll instance.
    ///
    /// Called just before `close(fd)`. Errors are logged but not fatal; the
    /// fd may already be invalid if the OS closed it (e.g. remote RST).
    pub fn delete(&self, fd: RawFd) -> Result<(), EpollError> {
        // Linux ≥ 2.6.9: the event pointer may be NULL for DEL.
        let rc = unsafe { epoll_ctl(self.fd, EPOLL_CTL_DEL, fd, std::ptr::null_mut()) };
        if rc < 0 {
            return Err(EpollError::CtlFailed { op: "DEL", fd, errno: errno() });
        }
        Ok(())
    }

    /// Block until at least one event fires or `timeout_ms` elapses.
    ///
    /// Returns the number of events written into `events`.
    /// Returns `Ok(0)` on timeout.
    ///
    /// `EINTR` (signal interruption) is transparently retried so callers
    /// never see spurious errors from signal delivery.
    pub fn wait(
        &self,
        events:     &mut [epoll_event],
        timeout_ms: i32,
    ) -> Result<usize, EpollError> {
        loop {
            let n = unsafe {
                epoll_wait(
                    self.fd,
                    events.as_mut_ptr(),
                    events.len() as i32,
                    timeout_ms,
                )
            };
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = errno();
            if e == libc::EINTR {
                // Interrupted by a signal — retry immediately.
                continue;
            }
            return Err(EpollError::WaitFailed(e));
        }
    }

    /// Return the raw epoll file descriptor (needed for CGI pipe registration).
    pub fn raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Drop for Epoll {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

// Epoll fd is just an integer; there is no aliasing as long as we never clone.
// The fd is not Send across threads in the general case, but since we are
// single-threaded this marker impl is safe.
unsafe impl Send for Epoll {}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Construct an `epoll_event` with the given event mask and opaque token.
///
/// The union field `data.u64` stores the token (always == fd as u64 in our
/// design) so dispatch is a direct array lookup.
#[inline]
fn make_event(events: u32, token: u64) -> epoll_event {
    // epoll_event contains a union; we must initialise it fully.
    // SAFETY: zero-initialising a C union is always valid.
    let mut ev: epoll_event = unsafe { std::mem::zeroed() };
    ev.events = events;
    ev.u64    = token;
    ev
}

/// Read `errno` from the C library after a failed syscall.
#[inline]
fn errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

// ---------------------------------------------------------------------------
// Event flag helpers (used by dispatcher)
// ---------------------------------------------------------------------------

/// Returns `true` if `events` contains `EPOLLIN`.
#[inline]
pub fn is_readable(events: u32) -> bool {
    events & EPOLLIN as u32 != 0
}

/// Returns `true` if `events` contains `EPOLLOUT`.
#[inline]
pub fn is_writable(events: u32) -> bool {
    events & EPOLLOUT as u32 != 0
}

/// Returns `true` if the connection should be closed:
/// `EPOLLERR`, `EPOLLHUP`, or `EPOLLRDHUP`.
#[inline]
pub fn is_closed(events: u32) -> bool {
    events & (EPOLLERR as u32 | EPOLLHUP as u32 | EPOLLRDHUP as u32) != 0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_returns_valid_epoll() {
        let ep = Epoll::create().expect("epoll_create1 should succeed");
        assert!(ep.fd >= 0);
    }

    #[test]
    fn add_and_delete_listener() {
        // Create a real socket pair to get a valid fd.
        let (rd, wr) = make_pipe();
        let ep = Epoll::create().unwrap();

        ep.add(rd, EPOLLIN as u32, rd as u64).expect("add should succeed");
        ep.delete(rd).expect("delete should succeed");

        close_fds(rd, wr);
    }

    #[test]
    fn modify_changes_event_mask() {
        let (rd, wr) = make_pipe();
        let ep = Epoll::create().unwrap();

        ep.add(rd, EPOLLIN as u32, rd as u64).unwrap();
        ep.modify(rd, EPOLLOUT as u32, rd as u64).expect("modify should succeed");
        ep.delete(rd).unwrap();

        close_fds(rd, wr);
    }

    #[test]
    fn wait_timeout_returns_zero() {
        let ep = Epoll::create().unwrap();
        let mut events = vec![unsafe { std::mem::zeroed::<epoll_event>() }; 8];
        // 1 ms timeout on an empty set must return 0 immediately.
        let n = ep.wait(&mut events, 1).expect("wait should not fail");
        assert_eq!(n, 0);
    }

    #[test]
    fn wait_detects_readable_fd() {
        let (rd, wr) = make_pipe();
        let ep = Epoll::create().unwrap();
        ep.add(rd, EPOLLIN as u32, rd as u64).unwrap();

        // Write one byte so rd becomes readable.
        unsafe { libc::write(wr, b"x".as_ptr() as *const _, 1) };

        let mut events = vec![unsafe { std::mem::zeroed::<epoll_event>() }; 8];
        let n = ep.wait(&mut events, 100).unwrap();
        assert_eq!(n, 1);
        // epoll_event is a packed struct; copy fields to locals before
        // comparing to avoid unaligned-reference UB in assert_eq!.
        let token  = events[0].u64;
        let eflags = events[0].events;
        assert_eq!(token, rd as u64);
        assert!(is_readable(eflags));

        ep.delete(rd).unwrap();
        close_fds(rd, wr);
    }

    #[test]
    fn flag_helpers_correctness() {
        assert!( is_readable(EPOLLIN  as u32));
        assert!(!is_readable(EPOLLOUT as u32));
        assert!( is_writable(EPOLLOUT as u32));
        assert!(!is_writable(EPOLLIN  as u32));
        assert!( is_closed(EPOLLERR  as u32));
        assert!( is_closed(EPOLLHUP  as u32));
        assert!( is_closed(EPOLLRDHUP as u32));
        assert!(!is_closed(EPOLLIN   as u32));
    }

    #[test]
    fn double_delete_does_not_panic() {
        let (rd, wr) = make_pipe();
        let ep = Epoll::create().unwrap();
        ep.add(rd, EPOLLIN as u32, rd as u64).unwrap();
        ep.delete(rd).unwrap();
        // Second delete: fd is no longer in the set — epoll returns ENOENT.
        // We accept the error gracefully (caller logs it).
        let _ = ep.delete(rd);
        close_fds(rd, wr);
    }

    // ---- test helpers ------------------------------------------------------

    fn make_pipe() -> (RawFd, RawFd) {
        let mut fds = [0i32; 2];
        unsafe { libc::pipe(fds.as_mut_ptr()) };
        (fds[0], fds[1])
    }

    fn close_fds(a: RawFd, b: RawFd) {
        unsafe {
            libc::close(a);
            libc::close(b);
        }
    }
}