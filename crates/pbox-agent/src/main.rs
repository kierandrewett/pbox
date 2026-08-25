use anyhow::{Context, Result};
use clap::Parser;
use ipnet::IpNet;
use nix::libc;
use pbox_proto::PROTOCOL_VERSION as PROTOCOL;
use pbox_proto::agent::agent_server::{Agent, AgentServer};
use pbox_proto::agent::{
    ExecEvent, ExecExit, ExecRequest, FileChunk, FileResult, ForwardClose, ForwardEvent,
    GetFileRequest, InfoRequest, InfoResponse, PingRequest, PingResponse, exec_event,
    forward_event,
};
use rustls::RootCertStore;
use rustls::server::WebPkiClientVerifier;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::transport::Server;
use tonic::transport::server::{Connected, TcpConnectInfo};
use tonic::{Request, Response, Status, Streaming};
const MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;
const FORWARD_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const FILE_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const FILE_STREAM_POST_EOF_TIMEOUT: Duration = Duration::from_millis(10);
const FORWARD_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const FORWARD_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const EXEC_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const EXEC_STDIN_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const EXEC_OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
static NEXT_UPLOAD_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Parser)]
#[command(name = "pbox-agent", about = "Authenticated pbox guest agent")]
struct Args {
    #[arg(long, default_value = "0.0.0.0:7443")]
    listen: String,
    #[arg(long)]
    box_id: String,
    #[arg(long)]
    certificate: PathBuf,
    #[arg(long)]
    private_key: PathBuf,
    #[arg(long)]
    client_ca: PathBuf,
    #[arg(
        long = "forward-allow",
        value_delimiter = ',',
        default_value = "127.0.0.0/8,::1/128"
    )]
    forward_allow: Vec<IpNet>,
    #[arg(long, default_value_t = 32)]
    max_exec: usize,
    #[arg(long, default_value_t = 32)]
    max_forward: usize,
    #[arg(long, default_value_t = 32)]
    max_file: usize,
    #[arg(long, default_value_t = 64)]
    max_handshakes: usize,
}
type TlsHandshake =
    Pin<Box<dyn Future<Output = Result<TlsStream<LimitedTcpStream>, io::Error>> + Send>>;

struct LimitedIncoming {
    listener: TcpListener,
    permits: Arc<Semaphore>,
    tls_config: Arc<rustls::ServerConfig>,
    acquire: Option<
        Pin<
            Box<
                dyn Future<Output = Result<OwnedSemaphorePermit, tokio::sync::AcquireError>> + Send,
            >,
        >,
    >,
    permit: Option<OwnedSemaphorePermit>,
    handshake: Option<TlsHandshake>,
}

impl LimitedIncoming {
    fn new(
        listener: TcpListener,
        permits: Arc<Semaphore>,
        tls_config: Arc<rustls::ServerConfig>,
    ) -> Self {
        Self {
            listener,
            permits,
            tls_config,
            acquire: None,
            permit: None,
            handshake: None,
        }
    }
}

fn poll_tls_handshake(
    incoming: &mut LimitedIncoming,
    context: &mut TaskContext<'_>,
) -> Poll<Option<Result<TlsStream<LimitedTcpStream>, io::Error>>> {
    match incoming
        .handshake
        .as_mut()
        .expect("TLS handshake future exists")
        .as_mut()
        .poll(context)
    {
        Poll::Pending => Poll::Pending,
        Poll::Ready(result) => {
            incoming.handshake = None;
            Poll::Ready(Some(result))
        }
    }
}

impl Stream for LimitedIncoming {
    type Item = Result<TlsStream<LimitedTcpStream>, io::Error>;

    fn poll_next(self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.handshake.is_some() {
            return poll_tls_handshake(this, context);
        }
        if this.permit.is_none() {
            if this.acquire.is_none() {
                this.acquire = Some(Box::pin(this.permits.clone().acquire_owned()));
            }
            match this
                .acquire
                .as_mut()
                .expect("acquire future exists")
                .as_mut()
                .poll(context)
            {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(permit)) => {
                    this.acquire = None;
                    this.permit = Some(permit);
                }
                Poll::Ready(Err(_)) => return Poll::Ready(None),
            }
        }
        match this.listener.poll_accept(context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok((stream, _))) => {
                let limited_stream = LimitedTcpStream {
                    inner: stream,
                    _permit: this.permit.take().expect("connection permit exists"),
                };
                let acceptor = TlsAcceptor::from(this.tls_config.clone());
                this.handshake = Some(Box::pin(async move {
                    tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(limited_stream))
                        .await
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timed out")
                        })?
                        .map_err(io::Error::other)
                }));
                poll_tls_handshake(this, context)
            }
            Poll::Ready(Err(error)) => {
                this.permit = None;
                Poll::Ready(Some(Err(error)))
            }
        }
    }
}

struct LimitedTcpStream {
    inner: TcpStream,
    _permit: OwnedSemaphorePermit,
}

impl AsyncRead for LimitedTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl AsyncWrite for LimitedTcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

impl Connected for LimitedTcpStream {
    type ConnectInfo = TcpConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        TcpConnectInfo {
            local_addr: self.inner.local_addr().ok(),
            remote_addr: self.inner.peer_addr().ok(),
        }
    }
}

#[derive(Clone)]
struct AgentService {
    box_id: String,
    handshake_slots: Arc<Semaphore>,
    forward_slots: Arc<Semaphore>,
    file_slots: Arc<Semaphore>,
    forward_allow: Vec<IpNet>,
    exec_slots: Arc<Semaphore>,
}
#[tonic::async_trait]
impl Agent for AgentService {
    type ExecStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<ExecEvent, Status>> + Send>>;
    type GetFileStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<FileChunk, Status>> + Send>>;
    type ForwardStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<ForwardEvent, Status>> + Send>>;

    async fn info(&self, _request: Request<InfoRequest>) -> Result<Response<InfoResponse>, Status> {
        Ok(Response::new(InfoResponse {
            protocol_version: PROTOCOL,
            agent_version: env!("CARGO_PKG_VERSION").to_owned(),
            box_id: self.box_id.clone(),
            os_id: std::env::consts::OS.to_owned(),
            os_version: String::new(),
            architecture: std::env::consts::ARCH.to_owned(),
            capabilities: vec!["exec".to_owned(), "files".to_owned(), "forward".to_owned()],
        }))
    }

    async fn ping(&self, request: Request<PingRequest>) -> Result<Response<PingResponse>, Status> {
        if request.into_inner().protocol_version != PROTOCOL {
            return Err(Status::failed_precondition(
                "unsupported agent protocol version",
            ));
        }
        Ok(Response::new(PingResponse {
            protocol_version: PROTOCOL,
            box_id: self.box_id.clone(),
        }))
    }

    async fn exec(
        &self,
        request: Request<Streaming<ExecRequest>>,
    ) -> Result<Response<Self::ExecStream>, Status> {
        let handshake_permit = self
            .handshake_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("too many concurrent handshakes"))?;
        let mut requests = request.into_inner();
        let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, requests.message())
            .await
            .map_err(|_| Status::deadline_exceeded("exec handshake timed out"))??
            .ok_or_else(|| Status::invalid_argument("exec stream is empty"))?;
        validate_exec_request(&first)?;
        drop(handshake_permit);
        let permit = self
            .exec_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("too many concurrent exec requests"))?;
        let (sender, receiver) = mpsc::channel(16);
        tokio::spawn(run_command(first, requests, sender, permit));
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn put_file(
        &self,
        request: Request<Streaming<FileChunk>>,
    ) -> Result<Response<FileResult>, Status> {
        let handshake_permit = self
            .handshake_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("too many concurrent handshakes"))?;
        let mut stream = request.into_inner();
        let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.message())
            .await
            .map_err(|_| Status::deadline_exceeded("file handshake timed out"))??
            .ok_or_else(|| Status::invalid_argument("file stream is empty"))?;
        if first.protocol_version != PROTOCOL {
            return Err(Status::failed_precondition(
                "unsupported agent protocol version",
            ));
        }
        drop(handshake_permit);
        let _file_permit = self
            .file_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("too many concurrent file requests"))?;
        let path = safe_path(&first.path)?;
        let temporary_path = temporary_path(&path);
        receive_upload(&mut stream, first, &temporary_path, &path)
            .await
            .map(Response::new)
    }

    async fn get_file(
        &self,
        request: Request<GetFileRequest>,
    ) -> Result<Response<Self::GetFileStream>, Status> {
        let request = request.into_inner();
        if request.protocol_version != PROTOCOL {
            return Err(Status::failed_precondition(
                "unsupported agent protocol version",
            ));
        }
        let file_permit = self
            .file_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("too many concurrent file requests"))?;
        let path = safe_path(&request.path)?;
        let (sender, receiver) = mpsc::channel(16);
        tokio::spawn(async move {
            let _file_permit = file_permit;
            read_file(path, sender).await;
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn forward(
        &self,
        request: Request<Streaming<ForwardEvent>>,
    ) -> Result<Response<Self::ForwardStream>, Status> {
        let handshake_permit = self
            .handshake_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("too many concurrent handshakes"))?;
        let mut inbound = request.into_inner();
        let open_event = tokio::time::timeout(HANDSHAKE_TIMEOUT, inbound.message())
            .await
            .map_err(|_| Status::deadline_exceeded("forward handshake timed out"))??
            .ok_or_else(|| Status::invalid_argument("forward stream must start with open"))?;
        let forward_event::Event::Open(open) = open_event
            .event
            .ok_or_else(|| Status::invalid_argument("forward stream missing open event"))?
        else {
            return Err(Status::invalid_argument(
                "forward stream must start with open",
            ));
        };
        if open.protocol_version != PROTOCOL {
            return Err(Status::failed_precondition(
                "unsupported agent protocol version",
            ));
        }
        if open.host.is_empty() || open.port == 0 || open.port > u16::MAX as u32 {
            return Err(Status::invalid_argument("invalid forward target"));
        }
        drop(handshake_permit);
        let forward_permit = self
            .forward_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("too many concurrent forwards"))?;
        let port = open.port as u16;
        let addresses = tokio::time::timeout(
            FORWARD_CONNECT_TIMEOUT,
            tokio::net::lookup_host((open.host.as_str(), port)),
        )
        .await
        .map_err(|_| Status::deadline_exceeded("forward target lookup timed out"))?
        .map_err(|error| Status::unavailable(format!("resolve forward target: {error}")))?
        .collect::<Vec<_>>();
        let target = addresses
            .into_iter()
            .find(|address| is_forward_address_allowed(&self.forward_allow, address.ip()))
            .ok_or_else(|| Status::permission_denied("forward target is not allowed"))?;
        let socket = tokio::time::timeout(FORWARD_CONNECT_TIMEOUT, TcpStream::connect(target))
            .await
            .map_err(|_| Status::deadline_exceeded("connect forward target timed out"))?
            .map_err(|error| Status::unavailable(format!("connect forward target: {error}")))?;
        let (mut reader, mut writer) = socket.into_split();
        let (sender, receiver) = mpsc::channel(32);
        tokio::spawn(async move {
            let _forward_permit = forward_permit;
            let mut buffer = [0u8; 8192];
            let mut close_code = 0;
            loop {
                tokio::select! {
                    message = tokio::time::timeout(FORWARD_IDLE_TIMEOUT, inbound.message()) => {
                        let message = match message {
                            Ok(Ok(Some(message))) => message,
                            Ok(Ok(None)) => {
                                close_code = 1;
                                break;
                            }
                            Ok(Err(_)) => {
                                close_code = 1;
                                break;
                            }
                            Err(_) => {
                                close_code = 2;
                                break;
                            }
                        };
                        match message.event {
                            Some(forward_event::Event::Data(data)) => {
                                match tokio::time::timeout(FORWARD_WRITE_TIMEOUT, writer.write_all(&data)).await {
                                    Ok(Ok(())) => {}
                                    _ => {
                                        close_code = 1;
                                        break;
                                    }
                                }
                            }
                            Some(forward_event::Event::Close(close)) => {
                                close_code = close.code;
                                break;
                            }
                            Some(forward_event::Event::Open(_)) | None => {
                                close_code = 1;
                                break;
                            }
                        }
                    }
                    result = tokio::time::timeout(FORWARD_IDLE_TIMEOUT, reader.read(&mut buffer)) => {
                        match result {
                            Ok(Ok(0)) => break,
                            Ok(Ok(size)) => {
                                let event = Ok(ForwardEvent {
                                    event: Some(forward_event::Event::Data(
                                        buffer[..size].to_vec(),
                                    )),
                                });
                                if !matches!(
                                    tokio::time::timeout(FORWARD_IDLE_TIMEOUT, sender.send(event))
                                        .await,
                                    Ok(Ok(()))
                                ) {
                                    return;
                                }
                            }
                            Ok(Err(_)) => {
                                close_code = 1;
                                break;
                            }
                            Err(_) => {
                                close_code = 2;
                                break;
                            }
                        }
                    }
                }
            }
            let _ = tokio::time::timeout(
                FORWARD_IDLE_TIMEOUT,
                sender.send(Ok(ForwardEvent {
                    event: Some(forward_event::Event::Close(ForwardClose {
                        code: close_code,
                    })),
                })),
            )
            .await;
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

fn validate_exec_request(request: &ExecRequest) -> Result<(), Status> {
    if request.protocol_version != PROTOCOL {
        return Err(Status::failed_precondition(
            "unsupported agent protocol version",
        ));
    }
    if request.argv.is_empty() || request.argv[0].is_empty() {
        return Err(Status::invalid_argument(
            "argv must contain at least one command",
        ));
    }
    Ok(())
}

async fn run_command(
    request: ExecRequest,
    requests: Streaming<ExecRequest>,
    sender: mpsc::Sender<Result<ExecEvent, Status>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
) {
    if request.allocate_pty {
        run_pty_command(request, requests, sender).await;
    } else {
        run_piped_command(request, requests, sender).await;
    }
}

async fn run_piped_command(
    request: ExecRequest,
    requests: Streaming<ExecRequest>,
    sender: mpsc::Sender<Result<ExecEvent, Status>>,
) {
    let mut command = command_for_request(&request);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    set_process_group(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = sender
                .send(Err(Status::internal(format!("spawn command: {error}"))))
                .await;
            return;
        }
    };
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut input_task = tokio::spawn(forward_stdin(
        requests,
        stdin,
        request.stdin,
        request.stdin_eof,
    ));
    let output_failure = Arc::new(Notify::new());
    let stdout_task = tokio::spawn(send_output(
        stdout,
        sender.clone(),
        true,
        false,
        output_failure.clone(),
    ));
    let stderr_task = tokio::spawn(send_output(
        stderr,
        sender.clone(),
        false,
        false,
        output_failure.clone(),
    ));
    let (status, input_error, input_finished) =
        wait_for_child_with_input(&mut child, &sender, &mut input_task, true, &output_failure)
            .await;
    if !input_finished {
        input_task.abort();
        let _ = input_task.await;
    }
    await_output_task(stdout_task).await;
    await_output_task(stderr_task).await;
    if let Some(error) = input_error {
        let _ = tokio::time::timeout(EXEC_OUTPUT_DRAIN_TIMEOUT, sender.send(Err(error))).await;
    } else {
        let _ = tokio::time::timeout(EXEC_OUTPUT_DRAIN_TIMEOUT, send_exit(status, sender)).await;
    }
}

async fn run_pty_command(
    request: ExecRequest,
    requests: Streaming<ExecRequest>,
    sender: mpsc::Sender<Result<ExecEvent, Status>>,
) {
    let pty = match nix::pty::openpty(None, None) {
        Ok(pty) => pty,
        Err(error) => {
            let _ = sender
                .send(Err(Status::internal(format!("create PTY: {error}"))))
                .await;
            return;
        }
    };
    let slave = std::fs::File::from(pty.slave);
    let slave_fd = slave.as_raw_fd();
    let slave_stdout = match slave.try_clone() {
        Ok(file) => file,
        Err(error) => {
            let _ = sender.send(Err(internal_io(error))).await;
            return;
        }
    };
    let slave_stderr = match slave.try_clone() {
        Ok(file) => file,
        Err(error) => {
            let _ = sender.send(Err(internal_io(error))).await;
            return;
        }
    };
    let mut command = command_for_request(&request);
    command
        .stdin(Stdio::from(slave))
        .stdout(Stdio::from(slave_stdout))
        .stderr(Stdio::from(slave_stderr));
    unsafe {
        command.as_std_mut().pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let master = std::fs::File::from(pty.master);
    let master_reader = match master.try_clone() {
        Ok(file) => tokio::fs::File::from_std(file),
        Err(error) => {
            let _ = sender.send(Err(internal_io(error))).await;
            return;
        }
    };
    let master_writer = tokio::fs::File::from_std(master);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = sender
                .send(Err(Status::internal(format!("spawn PTY command: {error}"))))
                .await;
            return;
        }
    };
    let mut input_task = tokio::spawn(forward_stdin(
        requests,
        Some(master_writer),
        request.stdin,
        request.stdin_eof,
    ));
    let output_failure = Arc::new(Notify::new());
    let output_task = tokio::spawn(send_output(
        Some(master_reader),
        sender.clone(),
        true,
        true,
        output_failure.clone(),
    ));
    let (status, input_error, input_finished) =
        wait_for_child_with_input(&mut child, &sender, &mut input_task, true, &output_failure)
            .await;
    if !input_finished {
        input_task.abort();
        let _ = input_task.await;
    }
    await_output_task(output_task).await;
    if let Some(error) = input_error {
        let _ = tokio::time::timeout(EXEC_OUTPUT_DRAIN_TIMEOUT, sender.send(Err(error))).await;
    } else {
        let _ = tokio::time::timeout(EXEC_OUTPUT_DRAIN_TIMEOUT, send_exit(status, sender)).await;
    }
}

fn command_for_request(request: &ExecRequest) -> Command {
    let requested_user = if request.user.is_empty() {
        "pbox"
    } else {
        request.user.as_str()
    };
    let mut command = if requested_user == "root" {
        let mut command = Command::new(&request.argv[0]);
        command.args(&request.argv[1..]);
        command
    } else {
        let mut command = Command::new("/usr/bin/sudo");
        command
            .arg("-n")
            .arg("-u")
            .arg(requested_user)
            .arg("--")
            .arg(&request.argv[0]);
        command.args(&request.argv[1..]);
        command
    };
    if !request.cwd.is_empty() {
        command.current_dir(&request.cwd);
    }
    command.envs(&request.env);
    if requested_user != "root" {
        command.env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        );
    }
    command
}
fn set_process_group(command: &mut Command) {
    unsafe {
        command.as_std_mut().pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}
async fn forward_stdin<W>(
    mut requests: Streaming<ExecRequest>,
    mut writer: Option<W>,
    initial_data: Vec<u8>,
    initial_eof: bool,
) -> Result<(), Status>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let Some(mut writer) = writer.take() else {
        return Ok(());
    };
    if !initial_data.is_empty() {
        tokio::time::timeout(EXEC_STDIN_WRITE_TIMEOUT, writer.write_all(&initial_data))
            .await
            .map_err(|_| Status::deadline_exceeded("stdin write timed out"))?
            .map_err(internal_io)?;
    }
    if initial_eof {
        tokio::time::timeout(EXEC_STDIN_WRITE_TIMEOUT, writer.shutdown())
            .await
            .map_err(|_| Status::deadline_exceeded("stdin shutdown timed out"))?
            .map_err(internal_io)?;
        return Ok(());
    }
    while let Some(request) = tokio::time::timeout(EXEC_STREAM_IDLE_TIMEOUT, requests.message())
        .await
        .map_err(|_| Status::deadline_exceeded("stdin stream idle timeout"))??
    {
        if request.protocol_version != PROTOCOL {
            return Err(Status::failed_precondition(
                "unsupported agent protocol version",
            ));
        }
        if !request.stdin.is_empty() {
            tokio::time::timeout(EXEC_STDIN_WRITE_TIMEOUT, writer.write_all(&request.stdin))
                .await
                .map_err(|_| Status::deadline_exceeded("stdin write timed out"))?
                .map_err(internal_io)?;
        }
        if request.stdin_eof {
            tokio::time::timeout(EXEC_STDIN_WRITE_TIMEOUT, writer.shutdown())
                .await
                .map_err(|_| Status::deadline_exceeded("stdin shutdown timed out"))?
                .map_err(internal_io)?;
            return Ok(());
        }
    }
    tokio::time::timeout(EXEC_STDIN_WRITE_TIMEOUT, writer.shutdown())
        .await
        .map_err(|_| Status::deadline_exceeded("stdin shutdown timed out"))?
        .map_err(internal_io)?;
    Ok(())
}

async fn wait_for_child(
    child: &mut tokio::process::Child,
    sender: &mpsc::Sender<Result<ExecEvent, Status>>,
    process_group: bool,
    output_failure: &Notify,
) -> std::io::Result<std::process::ExitStatus> {
    let process_group_id = child.id();
    tokio::select! {
        status = child.wait() => {
            if process_group && let Some(process_group_id) = process_group_id {
                kill_process_group(process_group_id);
            }
            status
        }
        _ = sender.closed() => {
            terminate_child(child, process_group).await;
            child.wait().await
        }
        _ = output_failure.notified() => {
            terminate_child(child, process_group).await;
            child.wait().await
        }
    }
}

fn kill_process_group(process_group_id: u32) -> bool {
    let Ok(process_group_id) = i32::try_from(process_group_id) else {
        return false;
    };
    unsafe { libc::kill(-process_group_id, libc::SIGKILL) == 0 }
}

async fn terminate_child(child: &mut tokio::process::Child, process_group: bool) {
    if process_group
        && let Some(process_group_id) = child.id()
        && kill_process_group(process_group_id)
    {
        return;
    }
    let _ = child.kill().await;
}

async fn wait_for_child_with_input(
    child: &mut tokio::process::Child,
    sender: &mpsc::Sender<Result<ExecEvent, Status>>,
    input_task: &mut tokio::task::JoinHandle<Result<(), Status>>,
    process_group: bool,
    output_failure: &Notify,
) -> (
    std::io::Result<std::process::ExitStatus>,
    Option<Status>,
    bool,
) {
    tokio::select! {
        status = wait_for_child(child, sender, process_group, output_failure) => (status, None, false),
        input_result = &mut *input_task => {
            match input_result {
                Ok(Ok(())) => (
                    wait_for_child(child, sender, process_group, output_failure).await,
                    None,
                    true,
                ),
                Ok(Err(error)) => {
                    terminate_child(child, process_group).await;
                    (child.wait().await, Some(error), true)
                }
                Err(error) => {
                    terminate_child(child, process_group).await;
                    (
                        child.wait().await,
                        Some(Status::internal(format!("stdin task failed: {error}"))),
                        true,
                    )
                }
            }
        }
    }
}

async fn send_exit(
    status: std::io::Result<std::process::ExitStatus>,
    sender: mpsc::Sender<Result<ExecEvent, Status>>,
) {
    match status {
        Ok(status) => {
            let _ = sender
                .send(Ok(ExecEvent {
                    event: Some(exec_event::Event::Exit(ExecExit {
                        code: status.code().unwrap_or(-1),
                        signal: exit_signal(&status),
                    })),
                }))
                .await;
        }
        Err(error) => {
            let _ = sender
                .send(Err(Status::internal(format!("wait for command: {error}"))))
                .await;
        }
    }
}

fn exit_signal(status: &std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().unwrap_or(0)
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        0
    }
}
async fn send_output<R>(
    reader: Option<R>,
    sender: mpsc::Sender<Result<ExecEvent, Status>>,
    stdout: bool,
    pty: bool,
    output_failure: Arc<Notify>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(mut reader) = reader else { return };
    let mut buffer = [0u8; 8192];
    loop {
        let size = match reader.read(&mut buffer).await {
            Ok(0) => return,
            Ok(size) => size,
            Err(error) if pty && error.raw_os_error() == Some(libc::EIO) => return,
            Err(error) => {
                let _ = tokio::time::timeout(
                    EXEC_STREAM_IDLE_TIMEOUT,
                    sender.send(Err(Status::internal(error.to_string()))),
                )
                .await;
                output_failure.notify_one();
                return;
            }
        };
        let event = if stdout {
            exec_event::Event::Stdout(buffer[..size].to_vec())
        } else {
            exec_event::Event::Stderr(buffer[..size].to_vec())
        };
        let sent = tokio::time::timeout(
            EXEC_STREAM_IDLE_TIMEOUT,
            sender.send(Ok(ExecEvent { event: Some(event) })),
        )
        .await;
        if !matches!(sent, Ok(Ok(()))) {
            output_failure.notify_one();
            return;
        }
    }
}
async fn await_output_task(mut task: tokio::task::JoinHandle<()>) {
    if tokio::time::timeout(EXEC_OUTPUT_DRAIN_TIMEOUT, &mut task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
}

struct TemporaryFileCleanup {
    path: PathBuf,
    armed: bool,
}

impl TemporaryFileCleanup {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TemporaryFileCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

async fn receive_upload(
    stream: &mut Streaming<FileChunk>,
    first: FileChunk,
    temporary_path: &Path,
    destination: &Path,
) -> Result<FileResult, Status> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options.open(temporary_path).await.map_err(internal_io)?;
    let mut cleanup = TemporaryFileCleanup::new(temporary_path);
    let mut size = 0u64;
    let mut hasher = Sha256::new();
    if first.uid != 0 || first.gid != 0 {
        return Err(Status::invalid_argument(
            "file ownership metadata is not supported",
        ));
    }
    write_chunk(&mut file, &first, &mut size, &mut hasher).await?;
    let mut complete = first.eof;
    while !complete {
        let Some(chunk) = tokio::time::timeout(FILE_STREAM_IDLE_TIMEOUT, stream.message())
            .await
            .map_err(|_| Status::deadline_exceeded("file stream idle timeout"))??
        else {
            return Err(Status::invalid_argument(
                "file stream ended before EOF marker",
            ));
        };
        if chunk.protocol_version != 0 && chunk.protocol_version != PROTOCOL {
            return Err(Status::failed_precondition(
                "unsupported agent protocol version",
            ));
        }
        if chunk.uid != 0 || chunk.gid != 0 {
            return Err(Status::invalid_argument(
                "file ownership metadata is not supported",
            ));
        }
        write_chunk(&mut file, &chunk, &mut size, &mut hasher).await?;
        complete = chunk.eof;
    }
    if tokio::time::timeout(FILE_STREAM_POST_EOF_TIMEOUT, stream.message())
        .await
        .map_err(|_| Status::deadline_exceeded("file stream post-EOF check timed out"))??
        .is_some()
    {
        return Err(Status::invalid_argument(
            "file stream sent data after EOF marker",
        ));
    }
    file.flush().await.map_err(internal_io)?;
    drop(file);
    if first.mode != 0 {
        set_mode(temporary_path, safe_mode(first.mode)).map_err(internal_io)?;
    }
    if first.atomic_write {
        tokio::fs::rename(temporary_path, destination)
            .await
            .map_err(internal_io)?;
    } else {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut destination_file = options.open(destination).await.map_err(internal_io)?;
        let mut source_file = tokio::fs::File::open(temporary_path)
            .await
            .map_err(internal_io)?;
        tokio::io::copy(&mut source_file, &mut destination_file)
            .await
            .map_err(internal_io)?;
        destination_file.flush().await.map_err(internal_io)?;
        if first.mode != 0 {
            set_mode(destination, safe_mode(first.mode)).map_err(internal_io)?;
        }
        tokio::fs::remove_file(temporary_path)
            .await
            .map_err(internal_io)?;
    }
    cleanup.disarm();
    Ok(FileResult {
        size,
        digest: hex_digest(hasher.finalize()),
    })
}

async fn write_chunk(
    file: &mut tokio::fs::File,
    chunk: &FileChunk,
    size: &mut u64,
    hasher: &mut Sha256,
) -> Result<(), Status> {
    let chunk_size = chunk.data.len() as u64;
    if chunk_size > MAX_FILE_SIZE.saturating_sub(*size) {
        return Err(Status::resource_exhausted("file exceeds the 64 MiB limit"));
    }
    file.write_all(&chunk.data).await.map_err(internal_io)?;
    *size += chunk_size;
    hasher.update(&chunk.data);
    Ok(())
}

async fn send_file_event(
    sender: &mpsc::Sender<Result<FileChunk, Status>>,
    event: Result<FileChunk, Status>,
) -> bool {
    matches!(
        tokio::time::timeout(FILE_STREAM_IDLE_TIMEOUT, sender.send(event)).await,
        Ok(Ok(()))
    )
}

async fn read_file(path: PathBuf, sender: mpsc::Sender<Result<FileChunk, Status>>) {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = match options.open(&path).await {
        Ok(file) => file,
        Err(error) => {
            let _ = send_file_event(&sender, Err(internal_io(error))).await;
            return;
        }
    };
    let metadata = match file.metadata().await {
        Ok(metadata) => metadata,
        Err(error) => {
            let _ = send_file_event(&sender, Err(internal_io(error))).await;
            return;
        }
    };
    if !metadata.is_file() {
        let _ = send_file_event(
            &sender,
            Err(Status::failed_precondition(
                "agent file reads require a regular file",
            )),
        )
        .await;
        return;
    }
    let mut buffer = [0u8; 8192];
    let mut first = true;
    let mut total = 0u64;
    loop {
        match file.read(&mut buffer).await {
            Ok(0) => {
                let event = Ok(FileChunk {
                    path: if first {
                        path.to_string_lossy().into_owned()
                    } else {
                        String::new()
                    },
                    mode: if first { metadata_mode(&metadata) } else { 0 },
                    protocol_version: if first { PROTOCOL } else { 0 },
                    eof: true,
                    ..Default::default()
                });
                let _ = send_file_event(&sender, event).await;
                return;
            }
            Ok(size) => {
                let chunk_size = size as u64;
                if chunk_size > MAX_FILE_SIZE.saturating_sub(total) {
                    let _ = send_file_event(
                        &sender,
                        Err(Status::resource_exhausted("file exceeds the 64 MiB limit")),
                    )
                    .await;
                    return;
                }
                total += chunk_size;
                let chunk = FileChunk {
                    path: if first {
                        path.to_string_lossy().into_owned()
                    } else {
                        String::new()
                    },
                    mode: if first { metadata_mode(&metadata) } else { 0 },
                    protocol_version: if first { PROTOCOL } else { 0 },
                    data: buffer[..size].to_vec(),
                    eof: false,
                    ..Default::default()
                };
                first = false;
                if !send_file_event(&sender, Ok(chunk)).await {
                    return;
                }
            }
            Err(error) => {
                let _ = send_file_event(&sender, Err(internal_io(error))).await;
                return;
            }
        }
    }
}

fn is_forward_address_allowed(networks: &[IpNet], address: IpAddr) -> bool {
    networks.iter().any(|network| network.contains(&address))
}

fn safe_path(value: &str) -> Result<PathBuf, Status> {
    if value.is_empty() || value.contains('\0') {
        return Err(Status::invalid_argument(
            "path must not be empty or contain NUL",
        ));
    }
    Ok(PathBuf::from(value))
}

fn temporary_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("upload");
    let upload_id = NEXT_UPLOAD_ID.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(
        ".{file_name}.pbox-upload-{}-{upload_id}",
        std::process::id()
    ))
}

fn metadata_mode(metadata: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

fn safe_mode(mode: u32) -> u32 {
    mode & 0o0777
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn internal_io(error: std::io::Error) -> Status {
    Status::internal(error.to_string())
}
fn build_server_tls_config(
    certificate_pem: &str,
    private_key_pem: &str,
    client_ca_pem: &str,
) -> Result<Arc<rustls::ServerConfig>> {
    let certificate = pem::parse(certificate_pem).context("parse server certificate PEM")?;
    if certificate.tag() != "CERTIFICATE" {
        anyhow::bail!("server certificate PEM has an unexpected tag");
    }
    let private_key = pem::parse(private_key_pem).context("parse server private key PEM")?;
    if private_key.tag() != "PRIVATE KEY" {
        anyhow::bail!("server private key PEM has an unexpected tag");
    }
    let mut roots = RootCertStore::empty();
    for certificate in pem::parse_many(client_ca_pem).context("parse client CA PEM")? {
        if certificate.tag() != "CERTIFICATE" {
            continue;
        }
        roots
            .add(CertificateDer::from(certificate.contents().to_vec()))
            .context("add client CA certificate")?;
    }
    if roots.is_empty() {
        anyhow::bail!("client CA PEM contains no certificates");
    }
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .context("build client certificate verifier")?;
    let private_key =
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key.contents().to_vec()));
    let mut config =
        rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                vec![CertificateDer::from(certificate.contents().to_vec())],
                private_key,
            )
            .context("build server TLS configuration")?;
    config.alpn_protocols = vec![b"h2".to_vec()];
    Ok(Arc::new(config))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.max_exec == 0 {
        anyhow::bail!("max-exec must be greater than zero");
    }
    if args.max_forward == 0 {
        anyhow::bail!("max-forward must be greater than zero");
    }
    if args.max_file == 0 {
        anyhow::bail!("max-file must be greater than zero");
    }
    if args.max_handshakes == 0 {
        anyhow::bail!("max-handshakes must be greater than zero");
    }
    let server_dns_name = pbox_crypto::server_dns_name(&args.box_id).context("validate box id")?;
    let certificate = tokio::fs::read_to_string(&args.certificate)
        .await
        .with_context(|| format!("read certificate {}", args.certificate.display()))?;
    let certificate_matches_box =
        pbox_crypto::certificate_has_dns_name(&certificate, &server_dns_name)
            .context("validate server certificate identity")?;
    if !certificate_matches_box {
        anyhow::bail!("server certificate does not contain DNS SAN {server_dns_name}");
    }
    let private_key = tokio::fs::read_to_string(&args.private_key)
        .await
        .with_context(|| format!("read private key {}", args.private_key.display()))?;
    let client_ca = tokio::fs::read_to_string(&args.client_ca)
        .await
        .with_context(|| format!("read client CA {}", args.client_ca.display()))?;
    let tls = build_server_tls_config(&certificate, &private_key, &client_ca)?;
    let exec_slots = Arc::new(Semaphore::new(args.max_exec));
    let file_slots = Arc::new(Semaphore::new(args.max_file));
    let handshake_slots = Arc::new(Semaphore::new(args.max_handshakes));
    let forward_slots = Arc::new(Semaphore::new(args.max_forward));
    let connection_slots = Arc::new(Semaphore::new(
        args.max_handshakes
            .saturating_add(args.max_exec)
            .saturating_add(args.max_file)
            .saturating_add(args.max_forward)
            .max(1),
    ));
    let address: SocketAddr = args.listen.parse().context("parse agent listen address")?;
    let listener = TcpListener::bind(address)
        .await
        .context("bind agent listen address")?;
    let incoming = LimitedIncoming::new(listener, connection_slots, tls);
    println!("pbox-agent listening on {}", args.listen);
    Server::builder()
        .add_service(AgentServer::new(AgentService {
            box_id: args.box_id,
            handshake_slots,
            file_slots,
            forward_slots,
            forward_allow: args.forward_allow,
            exec_slots,
        }))
        .serve_with_incoming(incoming)
        .await
        .context("run pbox-agent server")?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use pbox_agent_client::AgentClient;
    use pbox_crypto::{
        CertificatePurpose, derive_context_seed, generate_context_ca, issue_certificate,
        server_subject,
    };
    use tokio::net::TcpListener;

    async fn spawn_test_agent(
        box_id: &str,
        server_certificate: pbox_crypto::CertificateMaterial,
        client_ca: &pbox_crypto::CertificateMaterial,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let service = AgentService {
            box_id: box_id.to_owned(),
            handshake_slots: Arc::new(Semaphore::new(4)),
            forward_slots: Arc::new(Semaphore::new(4)),
            file_slots: Arc::new(Semaphore::new(4)),
            forward_allow: vec!["127.0.0.0/8".parse().unwrap()],
            exec_slots: Arc::new(Semaphore::new(4)),
        };
        let tls = build_server_tls_config(
            &server_certificate.certificate_pem,
            &server_certificate.private_key_pem,
            &client_ca.certificate_pem,
        )
        .unwrap();
        let incoming = LimitedIncoming::new(listener, Arc::new(Semaphore::new(16)), tls);
        let task = tokio::spawn(async move {
            Server::builder()
                .add_service(AgentServer::new(service))
                .serve_with_incoming(incoming)
                .await
                .unwrap();
        });
        (format!("https://{address}"), task)
    }

    async fn stop_test_agent(task: tokio::task::JoinHandle<()>) {
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn local_agent_supports_authenticated_info_and_exec() {
        let box_id = "pbx_t3yzd9y3";
        let seed = derive_context_seed("pbox@pve!cli", "secret");
        let ca = generate_context_ca(&seed).unwrap();
        let server = issue_certificate(
            &ca,
            &server_subject(box_id).unwrap(),
            CertificatePurpose::Server,
        )
        .unwrap();
        let client = issue_certificate(
            &ca,
            "pbox.cwd.dev/context/test-client",
            CertificatePurpose::Client,
        )
        .unwrap();
        let (endpoint, task) = spawn_test_agent(box_id, server, &ca).await;
        let mut agent = AgentClient::connect(&endpoint, box_id, &ca.certificate_pem, &client)
            .await
            .unwrap();

        let info = agent.info().await.unwrap();
        assert_eq!(info.protocol_version, PROTOCOL);
        assert_eq!(info.box_id, box_id);
        assert!(
            info.capabilities
                .iter()
                .any(|capability| capability == "exec")
        );

        let result = agent
            .exec(
                vec![
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    "printf agent-smoke".to_owned(),
                ],
                "/tmp",
                [],
                "root",
            )
            .await
            .unwrap();
        assert_eq!(result.stdout, b"agent-smoke");
        assert!(result.stderr.is_empty());
        assert_eq!(result.code, 0);
        assert!(result.exited);

        stop_test_agent(task).await;
    }

    #[tokio::test]
    async fn wrong_context_cannot_connect_to_agent() {
        let box_id = "pbx_t3yzd9y3";
        let server_seed = derive_context_seed("pbox@pve!cli", "secret");
        let server_ca = generate_context_ca(&server_seed).unwrap();
        let server = issue_certificate(
            &server_ca,
            &server_subject(box_id).unwrap(),
            CertificatePurpose::Server,
        )
        .unwrap();
        let client_seed = derive_context_seed("pbox@pve!cli", "other-secret");
        let client_ca = generate_context_ca(&client_seed).unwrap();
        let client = issue_certificate(
            &client_ca,
            "pbox.cwd.dev/context/wrong/client",
            CertificatePurpose::Client,
        )
        .unwrap();
        let (endpoint, task) = spawn_test_agent(box_id, server, &server_ca).await;

        let result =
            AgentClient::connect(&endpoint, box_id, &client_ca.certificate_pem, &client).await;
        assert!(result.is_err());

        stop_test_agent(task).await;
    }

    #[tokio::test]
    async fn info_rejects_a_server_with_the_wrong_box_identity() {
        let expected_box_id = "pbx_t3yzd9y3";
        let served_box_id = "pbx_91mk2aa7";
        let seed = derive_context_seed("pbox@pve!cli", "secret");
        let ca = generate_context_ca(&seed).unwrap();
        let server = issue_certificate(
            &ca,
            &server_subject(expected_box_id).unwrap(),
            CertificatePurpose::Server,
        )
        .unwrap();
        let client = issue_certificate(
            &ca,
            "pbox.cwd.dev/context/test-client",
            CertificatePurpose::Client,
        )
        .unwrap();
        let (endpoint, task) = spawn_test_agent(served_box_id, server, &ca).await;
        let mut agent =
            AgentClient::connect(&endpoint, expected_box_id, &ca.certificate_pem, &client)
                .await
                .unwrap();

        let result = agent.info().await;
        assert!(matches!(
            result,
            Err(pbox_agent_client::AgentClientError::Identity(_))
        ));

        stop_test_agent(task).await;
    }

    #[test]
    fn default_forward_policy_allows_only_loopback() {
        let networks = vec![
            "127.0.0.0/8".parse::<IpNet>().unwrap(),
            "::1/128".parse::<IpNet>().unwrap(),
        ];

        assert!(is_forward_address_allowed(
            &networks,
            "127.0.0.1".parse().unwrap(),
        ));
        assert!(is_forward_address_allowed(
            &networks,
            "::1".parse().unwrap(),
        ));
        assert!(!is_forward_address_allowed(
            &networks,
            "10.0.0.1".parse().unwrap(),
        ));
    }

    #[test]
    fn safe_path_rejects_empty_and_nul_values() {
        assert!(safe_path("").is_err());
        assert!(safe_path("tmp\0file").is_err());
        assert!(safe_path("/tmp/file").is_ok());
    }

    #[test]
    fn safe_mode_strips_special_permission_bits() {
        assert_eq!(safe_mode(0o10755), 0o755);
        assert_eq!(safe_mode(0o100644), 0o644);
    }

    #[tokio::test]
    async fn completed_output_task_cleanup_does_not_poll_handle_twice() {
        await_output_task(tokio::spawn(async {})).await;
    }
}
