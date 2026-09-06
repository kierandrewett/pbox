//! Readiness-based PTY I/O, so detaching or cancelling never leaves a blocking read.
use nix::libc;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, unix::AsyncFd};

pub(super) struct PtyIo(AsyncFd<File>);

impl PtyIo {
    pub(super) fn new(file: File) -> io::Result<Self> {
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(AsyncFd::new(file)?))
    }
}

impl AsyncRead for PtyIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            let mut ready = ready!(self.0.poll_read_ready(cx))?;
            match ready.try_io(|fd| {
                let mut file = fd.get_ref();
                file.read(buffer.initialize_unfilled())
            }) {
                Ok(Ok(count)) => {
                    buffer.advance(count);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(error)) => return Poll::Ready(Err(error)),
                Err(_) => continue,
            }
        }
    }
}

impl AsyncWrite for PtyIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut ready = ready!(self.0.poll_write_ready(cx))?;
            match ready.try_io(|fd| {
                let mut file = fd.get_ref();
                file.write(bytes)
            }) {
                Ok(result) => return Poll::Ready(result),
                Err(_) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn an_idle_read_can_be_cancelled_and_resumed_with_the_slave_open() {
        let pair = nix::pty::openpty(None, None).unwrap();
        let mut slave = File::from(pair.slave);
        let mut master = PtyIo::new(File::from(pair.master)).unwrap();
        let mut byte = [0];
        assert!(
            tokio::time::timeout(Duration::from_millis(20), master.read_exact(&mut byte))
                .await
                .is_err()
        );
        slave.write_all(b"x").unwrap();
        tokio::time::timeout(Duration::from_secs(1), master.read_exact(&mut byte))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(byte, *b"x");
    }
}
