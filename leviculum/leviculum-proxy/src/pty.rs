use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Async wrapper around a PTY master file descriptor.
///
/// Uses tokio's `AsyncFd` to integrate a raw fd into the async runtime.
pub struct AsyncPty {
    inner: AsyncFd<OwnedFd>,
}

impl AsyncPty {
    /// Wrap a PTY master fd for async I/O.
    ///
    /// Sets the fd to non-blocking mode and registers it with tokio.
    pub fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        // Set non-blocking
        let raw = fd.as_raw_fd();
        let flags = nix::fcntl::fcntl(raw, nix::fcntl::FcntlArg::F_GETFL)
            .map_err(|e| io::Error::other(format!("fcntl F_GETFL: {e}")))?;
        let mut oflags = nix::fcntl::OFlag::from_bits_truncate(flags);
        oflags |= nix::fcntl::OFlag::O_NONBLOCK;
        nix::fcntl::fcntl(raw, nix::fcntl::FcntlArg::F_SETFL(oflags))
            .map_err(|e| io::Error::other(format!("fcntl F_SETFL: {e}")))?;

        let inner = AsyncFd::new(fd)?;
        Ok(Self { inner })
    }
}

impl AsyncRead for AsyncPty {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = match self.inner.poll_read_ready(cx) {
                Poll::Ready(Ok(guard)) => guard,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };

            let unfilled = buf.initialize_unfilled();

            match nix::unistd::read(self.inner.get_ref().as_raw_fd(), unfilled) {
                Ok(n) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Err(nix::errno::Errno::EAGAIN) => {
                    guard.clear_ready();
                    continue;
                }
                Err(e) => {
                    return Poll::Ready(Err(io::Error::other(format!("read: {e}"))));
                }
            }
        }
    }
}

impl AsyncWrite for AsyncPty {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = match self.inner.poll_write_ready(cx) {
                Poll::Ready(Ok(guard)) => guard,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };

            match nix::unistd::write(self.inner.get_ref(), buf) {
                Ok(n) => return Poll::Ready(Ok(n)),
                Err(nix::errno::Errno::EAGAIN) => {
                    guard.clear_ready();
                    continue;
                }
                Err(e) => {
                    return Poll::Ready(Err(io::Error::other(format!("write: {e}"))));
                }
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
