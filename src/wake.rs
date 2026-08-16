use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct WakeHandle {
    fd: Arc<OwnedFd>,
}

impl WakeHandle {
    pub(crate) fn new() -> io::Result<Self> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self { fd: Arc::new(fd) })
    }

    #[inline]
    pub(crate) fn wake(&self) {
        let value = 1_u64.to_ne_bytes();
        loop {
            let written =
                unsafe { libc::write(self.fd.as_raw_fd(), value.as_ptr().cast(), value.len()) };
            if written == value.len() as isize {
                return;
            }
            if written < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                // A saturated nonblocking eventfd is already readable, so the
                // main loop has all the notification it needs.
                if error.kind() == io::ErrorKind::WouldBlock {
                    return;
                }
            }
            return;
        }
    }

    pub(crate) fn drain(&self) -> io::Result<u64> {
        let mut value = 0_u64;
        loop {
            let read = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    (&mut value as *mut u64).cast(),
                    std::mem::size_of::<u64>(),
                )
            };
            if read == std::mem::size_of::<u64>() as isize {
                return Ok(value);
            }
            if read < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(0);
                }
                return Err(error);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short read from wake eventfd",
            ));
        }
    }

    #[inline]
    pub(crate) fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloned_wake_handle_coalesces_in_kernel_until_main_loop_drains() {
        let wake = WakeHandle::new().expect("eventfd should be available on Linux");
        let clone = wake.clone();

        wake.wake();
        clone.wake();
        assert_eq!(wake.drain().expect("eventfd drain should succeed"), 2);
        assert_eq!(wake.drain().expect("drained eventfd should be empty"), 0);

        clone.wake();
        assert_eq!(wake.drain().expect("eventfd should be reusable"), 1);
    }
}
