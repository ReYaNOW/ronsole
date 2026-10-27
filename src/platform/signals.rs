//! Process-signal shutdown through a self-pipe.
//!
//! SIGTERM, SIGINT, and SIGHUP run a handler that only writes one byte to a
//! nonblocking pipe. The main loop polls the read end and, when it is readable,
//! drains it and runs its normal shutdown path. No signal mask is changed, so
//! child shells inherit the default dispositions across fork/exec.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};

/// Signals that request a clean shutdown.
const SHUTDOWN_SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// Write end of the self-pipe, read by the async-signal handler. `-1` until installed.
static WRITE_FD: AtomicI32 = AtomicI32::new(-1);
/// Read end of the self-pipe, polled by the event loop. `-1` until installed.
static READ_FD: AtomicI32 = AtomicI32::new(-1);
/// Result of the first `install` call; later calls return it without repeating syscalls.
static INSTALL_RESULT: OnceLock<Result<(), String>> = OnceLock::new();

/// Creates the self-pipe and installs the shutdown handler once per process.
/// Repeated calls return the first result.
pub(crate) fn install() -> Result<(), String> {
    INSTALL_RESULT.get_or_init(install_once).clone()
}

/// Read end to add to a `poll` set. `-1` when signals are not installed, which
/// `poll` ignores.
pub(crate) fn wait_fd() -> libc::c_int {
    READ_FD.load(Ordering::Acquire)
}

/// Drains the self-pipe with one nonblocking read. Returns true when a shutdown
/// signal arrived since the last call.
pub(crate) fn take_shutdown_request() -> bool {
    let fd = wait_fd();
    if fd < 0 {
        return false;
    }
    let mut buffer = [0u8; 16];
    // SAFETY: `buffer` is valid for `buffer.len()` bytes and `fd` is the open
    // nonblocking read end created by `install_once`.
    let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
    read > 0
}

fn install_once() -> Result<(), String> {
    let mut fds: [libc::c_int; 2] = [-1, -1];
    // SAFETY: `fds` has room for the two descriptors `pipe2` writes.
    let pipe_result = unsafe {
        libc::pipe2(
            fds.as_mut_ptr(),
            libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if pipe_result != 0 {
        return Err(format!(
            "creating signal pipe failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let [read_fd, write_fd] = fds;
    WRITE_FD.store(write_fd, Ordering::Release);
    READ_FD.store(read_fd, Ordering::Release);

    for signal in SHUTDOWN_SIGNALS {
        // SAFETY: a zeroed `sigaction` is a valid starting value; the fields that
        // matter are set below before it is passed to `sigaction`.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = on_shutdown_signal as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        // SAFETY: `action.sa_mask` is a valid `sigset_t` inside `action`.
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        // SAFETY: `action` is fully initialized and the old-action pointer may be null.
        let result = unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
        if result != 0 {
            return Err(format!(
                "installing handler for signal {signal} failed: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

/// Signal handler: writes one byte to the self-pipe. Only async-signal-safe calls
/// are made here. `errno` is saved and restored so the interrupted code is unaffected.
extern "C" fn on_shutdown_signal(_signal: libc::c_int) {
    // SAFETY: `__errno_location` returns the calling thread's errno slot.
    let saved_errno = unsafe { *libc::__errno_location() };
    let fd = WRITE_FD.load(Ordering::Acquire);
    if fd >= 0 {
        let byte = 1u8;
        // A failed write (full pipe) is ignored: the pipe already holds a pending byte.
        // SAFETY: `byte` is valid for one byte and `fd` is the nonblocking write end.
        let _ = unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
    }
    // SAFETY: same thread-local errno slot as above.
    unsafe { *libc::__errno_location() = saved_errno };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readable_now(fd: libc::c_int) -> bool {
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pollfd` is one valid entry; a zero timeout never blocks.
        let ready = unsafe { libc::poll(&mut pollfd, 1, 0) };
        ready > 0 && pollfd.revents & libc::POLLIN != 0
    }

    #[test]
    fn sighup_wakes_pipe_and_is_drained_once() {
        install().expect("signal install should succeed on Linux");
        // A second install must not break the already-installed handler.
        install().expect("repeated signal install should return the cached result");

        let fd = wait_fd();
        assert!(fd >= 0, "installed pipe must expose a read end");
        assert!(!readable_now(fd), "pipe must start empty");

        // SAFETY: raising a signal with an installed handler only runs that handler.
        assert_eq!(unsafe { libc::raise(libc::SIGHUP) }, 0);

        assert!(readable_now(fd), "signal handler must make the pipe readable");
        assert!(take_shutdown_request(), "drain must report the signal");
        assert!(!readable_now(fd), "drain must empty the pipe");
        assert!(!take_shutdown_request(), "second drain must report nothing");
    }
}
