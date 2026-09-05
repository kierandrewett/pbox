use std::io;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Own the SSH process for exactly as long as its TLS connection exists.
pub(crate) enum AgentStream {
    Direct(TcpStream),
    Ssh {
        _child: Child,
        input: ChildStdin,
        output: ChildStdout,
    },
}

impl AgentStream {
    pub(crate) async fn connect(authority: &str, ssh_host: Option<&str>) -> io::Result<Self> {
        let Some(host) = ssh_host else {
            return TcpStream::connect(authority).await.map(Self::Direct);
        };
        if host.is_empty()
            || host.starts_with('-')
            || host.bytes().any(|b| b.is_ascii_whitespace() || b == 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid SSH host",
            ));
        }
        // OpenSSH -W carries the TLS byte stream without a local listening port.
        // https://man.openbsd.org/ssh.1#W
        let mut child = Command::new("ssh")
            .args([
                "-T",
                "-a",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-W",
                authority,
                "--",
                host,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let input = child.stdin.take().expect("piped SSH stdin");
        let output = child.stdout.take().expect("piped SSH stdout");
        Ok(Self::Ssh {
            _child: child,
            input,
            output,
        })
    }
}

impl AsyncRead for AgentStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Direct(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Ssh { output, .. } => Pin::new(output).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for AgentStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Direct(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Ssh { input, .. } => Pin::new(input).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Direct(stream) => Pin::new(stream).poll_flush(cx),
            Self::Ssh { input, .. } => Pin::new(input).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Direct(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Ssh { input, .. } => Pin::new(input).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn direct_transport_preserves_bytes_and_eof() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            socket.read_to_end(&mut data).await.unwrap();
            socket.write_all(&data).await.unwrap();
        });
        let mut stream = AgentStream::connect(&address, None).await.unwrap();
        stream.write_all(b"hello\0\xff").await.unwrap();
        stream.shutdown().await.unwrap();
        let mut data = Vec::new();
        stream.read_to_end(&mut data).await.unwrap();
        assert_eq!(data, b"hello\0\xff");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn invalid_ssh_host_is_rejected_before_starting_a_process() {
        for host in ["", "-oProxyCommand=bad", "host name", "host\n"] {
            assert!(
                AgentStream::connect("127.0.0.1:7443", Some(host))
                    .await
                    .is_err()
            );
        }
    }
}
