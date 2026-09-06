mod pty;
mod sessions;
mod terminal_host;
mod workspace;
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
use std::os::fd::RawFd;
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
use tokio_stream::{Stream, StreamExt, wrappers::ReceiverStream};
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
const PTY_OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
const EXEC_COMMAND_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const CONNECTION_MAX_AGE: Duration = Duration::from_secs(60 * 60);
const CONNECTION_MAX_AGE_GRACE: Duration = Duration::from_secs(30);
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
    /// Root-readable JSON containing the relay URL and this guest's scoped token.
    #[arg(long)]
    relay_config: Option<PathBuf>,
    /// Use a snapshot-scoped bootstrap route selected by the new PVE hostname.
    #[arg(long)]
    snapshot_bootstrap: bool,

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
type SemaphoreAcquire =
    Pin<Box<dyn Future<Output = Result<OwnedSemaphorePermit, tokio::sync::AcquireError>> + Send>>;

struct LimitedIncoming {
    listener: TcpListener,
    permits: Arc<Semaphore>,
    tls_config: Arc<rustls::ServerConfig>,
    acquire: Option<SemaphoreAcquire>,
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
    sessions: sessions::Sessions,
    terminal_host: Option<terminal_host::Client>,
}
#[tonic::async_trait]
impl Agent for AgentService {
    type TerminalStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<ExecEvent, Status>> + Send>>;
    type ExecStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<ExecEvent, Status>> + Send>>;
    type GetFileStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<FileChunk, Status>> + Send>>;
    type ForwardStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<ForwardEvent, Status>> + Send>>;

    async fn info(&self, _request: Request<InfoRequest>) -> Result<Response<InfoResponse>, Status> {
        let terminal_capabilities = if let Some(host) = &self.terminal_host {
            host.clone()
                .info(InfoRequest {})
                .await?
                .into_inner()
                .capabilities
        } else {
            vec![
                "terminal-history".to_owned(),
                "terminal-processes".to_owned(),
            ]
        };
        Ok(Response::new(InfoResponse {
            protocol_version: PROTOCOL,
            agent_version: env!("CARGO_PKG_VERSION").to_owned(),
            box_id: self.box_id.clone(),
            os_id: std::env::consts::OS.to_owned(),
            os_version: String::new(),
            architecture: std::env::consts::ARCH.to_owned(),
            agent_digest: agent_digest(),
            capabilities: vec![
                "exec".to_owned(),
                "pty".to_owned(),
                "terminal-sessions".to_owned(),
                "session-control".to_owned(),
                "files".to_owned(),
                "forward".to_owned(),
            ]
            .into_iter()
            .chain(
                ["terminal-history", "terminal-processes"]
                    .into_iter()
                    .filter(|cap| {
                        terminal_capabilities
                            .iter()
                            .any(|available| available == cap)
                    })
                    .map(str::to_owned),
            )
            .chain(
                (self.terminal_host.is_some()
                    && Path::new("/run/systemd/system").is_dir()
                    && std::env::var_os("PBOX_TERMINAL_SOCKET").is_none())
                .then(|| "terminal-owner-refresh".to_owned()),
            )
            .chain(
                self.terminal_host
                    .is_some()
                    .then(|| "durable-sessions".to_owned()),
            )
            .chain(
                Path::new("/etc/pbox/image.json")
                    .exists()
                    .then(|| "workspace".to_owned()),
            )
            .collect(),
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
        let mut first = workspace::resolve_request(first)?;
        first.session_name.clear(); // Exec always keeps its ordinary command lifetime.
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

    async fn terminal(
        &self,
        request: Request<Streaming<ExecRequest>>,
    ) -> Result<Response<Self::TerminalStream>, Status> {
        if let Some(mut host) = self.terminal_host.clone() {
            let input = request.into_inner().map_while(Result::ok);
            return Ok(Response::new(Box::pin(
                host.terminal(input).await?.into_inner(),
            )));
        }
        let _permit = self
            .handshake_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("too many concurrent handshakes"))?;
        let mut requests = request.into_inner();
        let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, requests.message())
            .await
            .map_err(|_| Status::deadline_exceeded("terminal handshake timed out"))??
            .ok_or_else(|| Status::invalid_argument("terminal stream is empty"))?;
        let first = workspace::resolve_request(first)?;
        validate_exec_request(&first)?;
        let stream = self.sessions.attach(first, requests, &self.exec_slots)?;
        Ok(Response::new(Box::pin(stream)))
    }

    async fn list_sessions(
        &self,
        request: Request<pbox_proto::agent::ListSessionsRequest>,
    ) -> Result<Response<pbox_proto::agent::ListSessionsResponse>, Status> {
        if let Some(mut host) = self.terminal_host.clone() {
            return host.list_sessions(request).await;
        }
        sessions::check_protocol(request.into_inner().protocol_version)?;
        Ok(Response::new(pbox_proto::agent::ListSessionsResponse {
            sessions: self.sessions.list(),
        }))
    }

    async fn start_session(
        &self,
        request: Request<ExecRequest>,
    ) -> Result<Response<pbox_proto::agent::TerminalSession>, Status> {
        if let Some(mut host) = self.terminal_host.clone() {
            return host.start_session(request).await;
        }
        let first = workspace::resolve_request(request.into_inner())?;
        validate_exec_request(&first)?;
        Ok(Response::new(
            self.sessions.start(first, &self.exec_slots).await?,
        ))
    }

    async fn read_session(
        &self,
        request: Request<pbox_proto::agent::SessionRequest>,
    ) -> Result<Response<pbox_proto::agent::ReadSessionResponse>, Status> {
        if let Some(mut host) = self.terminal_host.clone() {
            return host.read_session(request).await;
        }
        let request = request.into_inner();
        sessions::check_protocol(request.protocol_version)?;
        Ok(Response::new(
            self.sessions.read(&request.name, request.include_history)?,
        ))
    }

    async fn send_session(
        &self,
        request: Request<pbox_proto::agent::SendSessionRequest>,
    ) -> Result<Response<pbox_proto::agent::SendSessionResponse>, Status> {
        if let Some(mut host) = self.terminal_host.clone() {
            return host.send_session(request).await;
        }
        let request = request.into_inner();
        sessions::check_protocol(request.protocol_version)?;
        self.sessions
            .send(&request.name, &request.text, &request.keys)
            .await?;
        Ok(Response::new(pbox_proto::agent::SendSessionResponse {}))
    }

    async fn close_session(
        &self,
        request: Request<pbox_proto::agent::CloseSessionRequest>,
    ) -> Result<Response<pbox_proto::agent::CloseSessionResponse>, Status> {
        if let Some(mut host) = self.terminal_host.clone() {
            return host.close_session(request).await;
        }
        let request = request.into_inner();
        sessions::check_protocol(request.protocol_version)?;
        self.sessions.close(&request.name).await?;
        Ok(Response::new(pbox_proto::agent::CloseSessionResponse {}))
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
        if open.host.is_empty()
            || open.host.contains('\0')
            || open.port == 0
            || open.port > u16::MAX as u32
        {
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
            let mut client_write_closed = false;
            let mut target_write_closed = false;
            let close_code: u32;
            loop {
                if client_write_closed && target_write_closed {
                    let _ = tokio::time::timeout(
                        FORWARD_IDLE_TIMEOUT,
                        sender.send(Ok(ForwardEvent {
                            event: Some(forward_event::Event::Close(ForwardClose {
                                code: 0,
                                half_close: false,
                            })),
                        })),
                    )
                    .await;
                    return;
                }
                tokio::select! {
                    message = tokio::time::timeout(FORWARD_IDLE_TIMEOUT, inbound.message()), if !client_write_closed => {
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
                                if !matches!(
                                    tokio::time::timeout(FORWARD_WRITE_TIMEOUT, writer.write_all(&data))
                                        .await,
                                    Ok(Ok(()))
                                ) {
                                    close_code = 1;
                                    break;
                                }
                            }
                            Some(forward_event::Event::Close(close)) => {
                                if close.code != 0 {
                                    close_code = close.code;
                                    break;
                                }
                                if close.half_close {
                                    if !matches!(
                                        tokio::time::timeout(FORWARD_WRITE_TIMEOUT, writer.shutdown())
                                            .await,
                                        Ok(Ok(()))
                                    ) {
                                        close_code = 1;
                                        break;
                                    }
                                    client_write_closed = true;
                                } else {
                                    close_code = 0;
                                    break;
                                }
                            }
                            Some(forward_event::Event::Open(_)) | None => {
                                close_code = 1;
                                break;
                            }
                        }
                    }
                    result = tokio::time::timeout(FORWARD_IDLE_TIMEOUT, reader.read(&mut buffer)), if !target_write_closed => {
                        match result {
                            Ok(Ok(0)) => {
                                target_write_closed = true;
                                if !matches!(
                                    tokio::time::timeout(
                                        FORWARD_IDLE_TIMEOUT,
                                        sender.send(Ok(ForwardEvent {
                                            event: Some(forward_event::Event::Close(ForwardClose {
                                                code: 0,
                                                half_close: true,
                                            })),
                                        })),
                                    )
                                    .await,
                                    Ok(Ok(()))
                                ) {
                                    return;
                                }
                            }
                            Ok(Ok(size)) => {
                                if !matches!(
                                    tokio::time::timeout(
                                        FORWARD_IDLE_TIMEOUT,
                                        sender.send(Ok(ForwardEvent {
                                            event: Some(forward_event::Event::Data(
                                                buffer[..size].to_vec(),
                                            )),
                                        })),
                                    )
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
                        half_close: false,
                    })),
                })),
            )
            .await;
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

fn agent_digest() -> String {
    std::fs::read("/proc/self/exe")
        .ok()
        .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
        .unwrap_or_default()
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
    for (name, value) in &request.env {
        // Workspace commands drop credentials before exec and do not pass through
        // setuid sudo. Preserve image loader settings (for example CUDA paths).
        validate_environment_entry(
            name,
            value,
            request.user.as_str() == "root" || Path::new("/etc/pbox/image.json").exists(),
        )?;
    }
    Ok(())
}

fn validate_environment_entry(name: &str, value: &str, root: bool) -> Result<(), Status> {
    let mut characters = name.chars();
    let valid_start = characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic());
    if !valid_start
        || !characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return Err(Status::invalid_argument(
            "environment variable names must use shell identifier syntax",
        ));
    }
    if value.contains('\0') {
        return Err(Status::invalid_argument(
            "environment variable values cannot contain NUL bytes",
        ));
    }
    if !root && is_privileged_loader_environment(name) {
        return Err(Status::permission_denied(
            "loader environment variables are not allowed for non-root commands",
        ));
    }
    Ok(())
}

fn is_privileged_loader_environment(name: &str) -> bool {
    name.starts_with("LD_")
        || name.starts_with("DYLD_")
        || matches!(name, "GCONV_PATH" | "GLIBC_TUNABLES" | "LOCPATH")
}

async fn run_command(
    request: ExecRequest,
    requests: Streaming<ExecRequest>,
    sender: mpsc::Sender<Result<ExecEvent, Status>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
) {
    if request.allocate_pty {
        run_pty_command(request, requests, sender, None, None).await;
    } else {
        run_piped_command(request, requests, sender).await;
    }
}

async fn run_piped_command(
    request: ExecRequest,
    requests: Streaming<ExecRequest>,
    sender: mpsc::Sender<Result<ExecEvent, Status>>,
) {
    let deadline = Some(tokio::time::Instant::now() + EXEC_COMMAND_TIMEOUT);
    let mut command = match prepare_command(&request) {
        Ok(command) => command,
        Err(error) => {
            let _ = sender
                .send(Err(Status::invalid_argument(error.to_string())))
                .await;
            return;
        }
    };
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
        false,
        request.stdin,
        request.stdin_eof,
        None,
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
    let (status, input_error, input_finished) = wait_for_child_with_input(
        &mut child,
        &sender,
        &mut input_task,
        true,
        &output_failure,
        deadline,
    )
    .await;
    if !input_finished {
        input_task.abort();
        let _ = input_task.await;
    }
    await_output_task(stdout_task, EXEC_OUTPUT_DRAIN_TIMEOUT).await;
    await_output_task(stderr_task, EXEC_OUTPUT_DRAIN_TIMEOUT).await;
    if let Some(error) = input_error {
        let _ = tokio::time::timeout(EXEC_OUTPUT_DRAIN_TIMEOUT, sender.send(Err(error))).await;
    } else {
        let _ = tokio::time::timeout(EXEC_OUTPUT_DRAIN_TIMEOUT, send_exit(status, sender)).await;
    }
}

async fn pty_start_failure(
    sender: mpsc::Sender<Result<ExecEvent, Status>>,
    started: Option<tokio::sync::oneshot::Sender<Result<(), Status>>>,
    error: Status,
) {
    if let Some(started) = started {
        let _ = started.send(Err(error.clone()));
    }
    let _ = sender.send(Err(error)).await;
}

async fn run_pty_command(
    request: ExecRequest,
    requests: impl Stream<Item = Result<ExecRequest, Status>> + Unpin + Send + 'static,
    sender: mpsc::Sender<Result<ExecEvent, Status>>,
    mut started: Option<tokio::sync::oneshot::Sender<Result<(), Status>>>,
    session_pid: Option<Arc<std::sync::atomic::AtomicU32>>,
) {
    let deadline = request
        .session_name
        .is_empty()
        .then(|| tokio::time::Instant::now() + EXEC_COMMAND_TIMEOUT);
    let winsize = if request.terminal_rows > 0 && request.terminal_cols > 0 {
        Some(nix::pty::Winsize {
            ws_row: request.terminal_rows.min(u16::MAX as u32) as u16,
            ws_col: request.terminal_cols.min(u16::MAX as u32) as u16,
            ws_xpixel: 0,
            ws_ypixel: 0,
        })
    } else {
        None
    };
    let pty = match nix::pty::openpty(winsize.as_ref(), None) {
        Ok(pty) => pty,
        Err(error) => {
            pty_start_failure(
                sender,
                started.take(),
                Status::internal(format!("create PTY: {error}")),
            )
            .await;
            return;
        }
    };
    let slave = std::fs::File::from(pty.slave);
    let slave_fd = slave.as_raw_fd();
    let slave_stdout = match slave.try_clone() {
        Ok(file) => file,
        Err(error) => {
            pty_start_failure(sender, started.take(), internal_io(error)).await;
            return;
        }
    };
    let slave_stderr = match slave.try_clone() {
        Ok(file) => file,
        Err(error) => {
            pty_start_failure(sender, started.take(), internal_io(error)).await;
            return;
        }
    };
    let mut command = match prepare_command(&request) {
        Ok(command) => command,
        Err(error) => {
            pty_start_failure(
                sender,
                started.take(),
                Status::invalid_argument(error.to_string()),
            )
            .await;
            return;
        }
    };
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
    let master_fd = master.as_raw_fd();
    let master_reader = match master.try_clone().and_then(pty::PtyIo::new) {
        Ok(file) => file,
        Err(error) => {
            pty_start_failure(sender, started.take(), internal_io(error)).await;
            return;
        }
    };
    let master_writer = match pty::PtyIo::new(master) {
        Ok(file) => file,
        Err(error) => {
            pty_start_failure(sender, started.take(), internal_io(error)).await;
            return;
        }
    };
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            pty_start_failure(
                sender,
                started.take(),
                Status::internal(format!("spawn PTY command: {error}")),
            )
            .await;
            return;
        }
    };
    if let Some(pid) = session_pid {
        pid.store(
            child.id().unwrap_or(0),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    drop(command);
    if let Some(started) = started.take() {
        let _ = started.send(Ok(()));
    }
    let mut input_task = tokio::spawn(forward_stdin(
        requests,
        Some(master_writer),
        true,
        request.stdin,
        request.stdin_eof,
        Some(master_fd),
    ));
    let output_failure = Arc::new(Notify::new());
    let output_task = tokio::spawn(send_output(
        Some(master_reader),
        sender.clone(),
        true,
        true,
        output_failure.clone(),
    ));
    let (status, input_error, input_finished) = wait_for_child_with_input(
        &mut child,
        &sender,
        &mut input_task,
        true,
        &output_failure,
        deadline,
    )
    .await;
    if !input_finished {
        input_task.abort();
        let _ = input_task.await;
    }
    await_output_task(output_task, PTY_OUTPUT_DRAIN_TIMEOUT).await;
    if let Some(error) = input_error {
        let _ = tokio::time::timeout(EXEC_OUTPUT_DRAIN_TIMEOUT, sender.send(Err(error))).await;
    } else {
        let _ = tokio::time::timeout(EXEC_OUTPUT_DRAIN_TIMEOUT, send_exit(status, sender)).await;
    }
}

fn prepare_command(request: &ExecRequest) -> Result<Command> {
    if Path::new("/etc/pbox/image.json").exists() {
        return Ok(workspace::command(request)?.into());
    }
    Ok(command_for_request(request))
}
fn command_for_request(request: &ExecRequest) -> Command {
    let mut environment = request.env.clone();
    if request.allocate_pty {
        environment
            .entry("TERM".to_owned())
            .or_insert_with(|| "xterm-256color".to_owned());
    }
    let requested_user = if request.user.is_empty() {
        "pbox"
    } else {
        request.user.as_str()
    };
    let mut command = if requested_user == "root" {
        let mut command = Command::new(&request.argv[0]);
        command.args(&request.argv[1..]);
        command.envs(&environment);
        command
    } else {
        // Keep values out of sudo's argv. The validator rejects loader hooks before
        // this root process starts sudo, and --preserve-env applies the values after
        // sudo changes to the requested user.
        let mut command = Command::new("/usr/bin/sudo");
        command.arg("-n").arg("-u").arg(requested_user);
        let mut environment_names = environment.keys().cloned().collect::<Vec<_>>();
        if !environment.contains_key("PATH") {
            command.env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            );
            environment_names.push("PATH".to_owned());
        }
        environment_names.sort_unstable();
        if !environment_names.is_empty() {
            command.arg(format!("--preserve-env={}", environment_names.join(",")));
        }
        command.arg("--").args(&request.argv);
        command.envs(&environment);
        command
    };
    if !request.cwd.is_empty() {
        command.current_dir(&request.cwd);
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
async fn write_stdin<W>(writer: &mut W, data: &[u8], pty: bool) -> Result<(), Status>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let result = tokio::time::timeout(EXEC_STDIN_WRITE_TIMEOUT, writer.write_all(data))
        .await
        .map_err(|_| Status::deadline_exceeded("stdin write timed out"))?;
    match result {
        Ok(()) => Ok(()),
        Err(error) if pty && is_pty_eof_error(&error) => Ok(()),
        Err(error) => Err(internal_io(error)),
    }
}

fn is_pty_eof_error(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::EIO)
    }
    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

async fn forward_stdin<W>(
    mut requests: impl Stream<Item = Result<ExecRequest, Status>> + Unpin,
    mut writer: Option<W>,
    pty: bool,
    initial_data: Vec<u8>,
    initial_eof: bool,
    resize_fd: Option<RawFd>,
) -> Result<(), Status>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let Some(mut writer) = writer.take() else {
        return Ok(());
    };
    if !initial_data.is_empty() {
        write_stdin(&mut writer, &initial_data, pty).await?;
    }
    if initial_eof {
        finish_stdin(&mut writer, pty).await?;
        return Ok(());
    }
    while let Some(request) = tokio::time::timeout(EXEC_STREAM_IDLE_TIMEOUT, requests.next())
        .await
        .map_err(|_| Status::deadline_exceeded("stdin stream idle timeout"))?
        .transpose()?
    {
        if request.protocol_version != PROTOCOL {
            return Err(Status::failed_precondition(
                "unsupported agent protocol version",
            ));
        }
        if request.terminal_rows > 0
            && request.terminal_cols > 0
            && let Some(fd) = resize_fd
        {
            resize_pty(fd, request.terminal_rows, request.terminal_cols)?;
        }
        if !request.stdin.is_empty() {
            write_stdin(&mut writer, &request.stdin, pty).await?;
        }
        if request.stdin_eof {
            finish_stdin(&mut writer, pty).await?;
            return Ok(());
        }
    }
    finish_stdin(&mut writer, pty).await?;
    Ok(())
}

fn resize_pty(fd: RawFd, rows: u32, cols: u32) -> Result<(), Status> {
    let size = nix::libc::winsize {
        ws_row: rows.min(u16::MAX as u32) as u16,
        ws_col: cols.min(u16::MAX as u32) as u16,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe { nix::libc::ioctl(fd, nix::libc::TIOCSWINSZ, &size) };
    if result == -1 {
        return Err(internal_io(std::io::Error::last_os_error()));
    }
    Ok(())
}

async fn finish_stdin<W>(writer: &mut W, pty: bool) -> Result<(), Status>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let result = tokio::time::timeout(EXEC_STDIN_WRITE_TIMEOUT, async {
        if pty {
            // A PTY has no half-close. Use the terminal's conventional VEOF byte.
            writer.write_all(&[4]).await
        } else {
            writer.shutdown().await
        }
    })
    .await
    .map_err(|_| Status::deadline_exceeded("stdin shutdown timed out"))?;
    match result {
        Ok(()) => Ok(()),
        Err(error) if pty && is_pty_eof_error(&error) => Ok(()),
        Err(error) => Err(internal_io(error)),
    }
}

async fn wait_for_child(
    child: &mut tokio::process::Child,
    sender: &mpsc::Sender<Result<ExecEvent, Status>>,
    process_group: bool,
    output_failure: &Notify,
    deadline: Option<tokio::time::Instant>,
) -> (std::io::Result<std::process::ExitStatus>, Option<Status>) {
    let deadline_sleep = async {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(deadline_sleep);
    let process_group_id = child.id();
    tokio::select! {
        status = child.wait() => {
            if process_group && let Some(process_group_id) = process_group_id {
                kill_process_group(process_group_id);
            }
            (status, None)
        }
        _ = sender.closed() => {
            terminate_child(child, process_group).await;
            (child.wait().await, None)
        }
        _ = output_failure.notified() => {
            terminate_child(child, process_group).await;
            (child.wait().await, None)
        }
        _ = &mut deadline_sleep => {
            terminate_child(child, process_group).await;
            (
                child.wait().await,
                Some(Status::deadline_exceeded("command execution timed out")),
            )
        }
    }
}

fn kill_process_group(process_group_id: u32) -> bool {
    let Ok(process_group_id) = i32::try_from(process_group_id) else {
        return false;
    };
    let killed = unsafe { libc::kill(-process_group_id, libc::SIGKILL) == 0 };
    // Interactive shells put foreground and background jobs in separate process
    // groups. They still share the PTY's session ID; close those jobs as well.
    #[cfg(target_os = "linux")]
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some((_, fields)) = stat.rsplit_once(')') else {
                continue;
            };
            if fields
                .split_whitespace()
                .nth(3)
                .and_then(|sid| sid.parse::<i32>().ok())
                == Some(process_group_id)
            {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
    killed
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
    deadline: Option<tokio::time::Instant>,
) -> (
    std::io::Result<std::process::ExitStatus>,
    Option<Status>,
    bool,
) {
    tokio::select! {
        wait_result = wait_for_child(child, sender, process_group, output_failure, deadline) => {
            let (status, error) = wait_result;
            (status, error, false)
        }
        input_result = &mut *input_task => {
            match input_result {
                Ok(Ok(())) => {
                    let (status, error) =
                        wait_for_child(child, sender, process_group, output_failure, deadline)
                            .await;
                    (status, error, true)
                }
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
async fn await_output_task(mut task: tokio::task::JoinHandle<()>, timeout: Duration) {
    if tokio::time::timeout(timeout, &mut task).await.is_err() {
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

fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--terminal-host") {
        let path = std::env::args_os()
            .nth(2)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(terminal_host::SOCKET));
        let max_exec = std::env::args()
            .nth(3)
            .map(|value| value.parse::<usize>())
            .transpose()?
            .unwrap_or(32);
        anyhow::ensure!(max_exec > 0, "terminal limit must be greater than zero");
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(terminal_host::serve(&path, max_exec));
    }
    if std::env::args().nth(1).as_deref() == Some("--workspace-init") {
        return workspace::supervise();
    }
    if std::env::args().nth(1).as_deref() == Some("--workspace-service") {
        return workspace::service(false);
    }
    if std::env::args().nth(1).as_deref() == Some("--workspace-application") {
        return workspace::service(true);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve())
}

async fn serve() -> Result<()> {
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
    if let Some(path) = args.relay_config {
        let mut access: pbox_relay::RelayAccess =
            serde_json::from_slice(&tokio::fs::read(path).await?)
                .context("parse relay configuration")?;
        let route = if args.snapshot_bootstrap {
            let hostname = workspace::hostname();
            let suffix = hostname
                .trim()
                .strip_prefix("pbox-")
                .context("snapshot clone hostname must be pbox-ID")?;
            let route = format!("{}~pbx_{suffix}", args.box_id);
            anyhow::ensure!(
                pbox_relay::valid_route(&route),
                "invalid snapshot bootstrap route"
            );
            access.token = pbox_relay::scoped_token(&access.token, "agent", &route);
            route
        } else {
            args.box_id.clone()
        };
        pbox_relay::websocket_url(&access.url, "agent", &route)?;
        let mut local = listener.local_addr()?;
        if local.ip().is_unspecified() {
            local.set_ip(if local.is_ipv4() {
                std::net::Ipv4Addr::LOCALHOST.into()
            } else {
                std::net::Ipv6Addr::LOCALHOST.into()
            });
        }
        tokio::spawn(pbox_relay::run_agent(access, route, local));
    }
    let terminal_host = terminal_host::managed(args.max_exec).await?;
    let incoming = LimitedIncoming::new(listener, connection_slots, tls);
    println!("pbox-agent listening on {}", args.listen);
    Server::builder()
        .max_connection_age(CONNECTION_MAX_AGE)
        .max_connection_age_grace(CONNECTION_MAX_AGE_GRACE)
        .add_service(AgentServer::new(AgentService {
            box_id: args.box_id,
            handshake_slots,
            file_slots,
            forward_slots,
            forward_allow: args.forward_allow,
            exec_slots,
            sessions: sessions::Sessions::default(),
            terminal_host,
        }))
        .serve_with_incoming(incoming)
        .await
        .context("run pbox-agent server")?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use pbox_agent_client::{AgentClient, ExecInput};
    use pbox_crypto::{
        CertificatePurpose, derive_context_seed, generate_context_ca, issue_certificate,
        server_subject,
    };
    use tokio::net::{TcpListener, TcpStream};

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
            sessions: sessions::Sessions::default(),
            terminal_host: None,
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
    async fn relay_preserves_mutual_tls_and_guest_identity() {
        let box_id = "pbx_t3yzd9y3";
        let ca = generate_context_ca(&derive_context_seed("relay-test", "secret")).unwrap();
        let server = issue_certificate(
            &ca,
            &server_subject(box_id).unwrap(),
            CertificatePurpose::Server,
        )
        .unwrap();
        let identity =
            issue_certificate(&ca, "pbox.cwd.dev/context/test", CertificatePurpose::Client)
                .unwrap();
        let (endpoint, task) = spawn_test_agent(box_id, server, &ca).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let key = "relay-test-key-at-least-thirty-two-characters";
        let relay = tokio::spawn(pbox_relay::server::serve(listener, key.to_owned(), 8));
        let guest = pbox_relay::RelayAccess {
            url: url.clone(),
            token: pbox_relay::scoped_token(key, "agent", box_id),
        };
        let outbound = tokio::spawn(pbox_relay::run_agent(
            guest,
            box_id.to_owned(),
            endpoint.trim_start_matches("https://").parse().unwrap(),
        ));
        let access = pbox_relay::RelayAccess {
            url,
            token: pbox_relay::scoped_token(key, "client", box_id),
        };
        let mut client = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(client) = AgentClient::connect_with_relay(
                    "https://unreachable.invalid:443",
                    box_id,
                    &ca.certificate_pem,
                    &identity,
                    Some(&access),
                )
                .await
                {
                    break client;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(client.info().await.unwrap().box_id, box_id);
        let result = client
            .exec(
                vec!["/bin/echo".into(), "relay-ok".into()],
                "/",
                Vec::<(String, String)>::new(),
                "root",
            )
            .await
            .unwrap();
        assert_eq!(result.stdout, b"relay-ok\n");
        let other_ca = generate_context_ca(&derive_context_seed("other", "secret")).unwrap();
        assert!(
            AgentClient::connect_with_relay(
                "https://unreachable.invalid:443",
                box_id,
                &other_ca.certificate_pem,
                &identity,
                Some(&access)
            )
            .await
            .is_err()
        );
        outbound.abort();
        relay.abort();
        stop_test_agent(task).await;
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
        assert!(
            info.capabilities
                .iter()
                .any(|capability| capability == "pty")
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
    async fn terminal_sessions_survive_reconnection_resize_takeover_and_close() {
        let box_id = "pbx_t3yzd9y3";
        let ca = generate_context_ca(&derive_context_seed("sessions", "secret")).unwrap();
        let server = issue_certificate(
            &ca,
            &server_subject(box_id).unwrap(),
            CertificatePurpose::Server,
        )
        .unwrap();
        let identity = issue_certificate(
            &ca,
            "pbox.cwd.dev/context/test-client",
            CertificatePurpose::Client,
        )
        .unwrap();
        let (endpoint, task) = spawn_test_agent(box_id, server, &ca).await;
        let mut agent = AgentClient::connect(&endpoint, box_id, &ca.certificate_pem, &identity)
            .await
            .unwrap();
        let request = ExecRequest {
            argv: vec!["/bin/sh".into(), "-c".into(), "stty -echo; printf '__ready__\\n'; while IFS= read -r line; do eval \"$line\"; done".into()],
            cwd: "/tmp".into(), user: "root".into(), session_name: "main".into(), terminal_rows: 24, terminal_cols: 80,
            ..Default::default()
        };
        async fn until(session: &mut pbox_agent_client::ExecPtySession, marker: &str) -> String {
            tokio::time::timeout(Duration::from_secs(5), async {
                let mut output = Vec::new();
                loop {
                    let event = session
                        .output
                        .message()
                        .await
                        .unwrap()
                        .expect("session closed before marker");
                    if let Some(exec_event::Event::Stdout(bytes)) = event.event {
                        output.extend(bytes);
                    }
                    if String::from_utf8_lossy(&output).contains(marker) {
                        return String::from_utf8_lossy(&output).into_owned();
                    }
                }
            })
            .await
            .expect("terminal output timeout")
        }
        let mut first = agent.terminal_session(request.clone()).await.unwrap();
        until(&mut first, "__ready__").await;
        first
            .input
            .send(ExecInput::Data(
                b"value=retained; cd /; printf '__saved__\\n'\n".to_vec(),
            ))
            .await
            .unwrap();
        until(&mut first, "__saved__").await;
        drop(first);
        drop(agent);
        let mut agent = AgentClient::connect(&endpoint, box_id, &ca.certificate_pem, &identity)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let sessions = agent.list_sessions().await.unwrap();
                if sessions.len() == 1 && !sessions[0].attached {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut second = agent
            .terminal_session(ExecRequest {
                argv: vec!["/bin/false".into()],
                ..request.clone()
            })
            .await
            .unwrap();
        until(&mut second, "__saved__").await;
        second
            .input
            .send(ExecInput::Resize {
                rows: 40,
                cols: 100,
            })
            .await
            .unwrap();
        second
            .input
            .send(ExecInput::Data(
                b"printf '__state__%s:%s\\n' \"$value\" \"$PWD\"; stty size\n".to_vec(),
            ))
            .await
            .unwrap();
        let output = until(&mut second, "40 100").await;
        assert!(output.contains("__state__retained:/"), "{output:?}");
        let sessions = agent.list_sessions().await.unwrap();
        assert_eq!((sessions[0].rows, sessions[0].cols), (40, 100));
        assert!(sessions[0].attached);

        // A fresh connection moves the attachment; it does not start another shell.
        let mut third = agent.terminal_session(request.clone()).await.unwrap();
        until(&mut third, "__state__retained:/").await;
        // Already queued output can precede the takeover status (even the CRLF
        // after the marker may arrive in another transport frame).
        let error = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match second.output.message().await {
                    Ok(Some(_)) => continue,
                    Err(error) => break error,
                    Ok(None) => panic!("old attachment ended without its cancellation status"),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(error.code(), tonic::Code::Cancelled);
        let wrong_user = agent
            .terminal_session(ExecRequest {
                user: "different-user".into(),
                ..request.clone()
            })
            .await;
        assert!(wrong_user.is_err());
        // Named terminals share the execution limit, even while detached.
        for name in ["one", "two", "three"] {
            let session = agent
                .terminal_session(ExecRequest {
                    session_name: name.into(),
                    ..request.clone()
                })
                .await
                .unwrap();
            drop(session);
        }
        assert!(
            agent
                .terminal_session(ExecRequest {
                    session_name: "overflow".into(),
                    ..request.clone()
                })
                .await
                .is_err()
        );
        for name in ["one", "two", "three", "main"] {
            agent.close_session(name).await.unwrap();
        }
        assert!(agent.list_sessions().await.unwrap().is_empty());

        // Output must keep draining while nobody is attached, including beyond
        // the viewer channel's capacity. Reattach shows the latest screen.
        let flood_request = ExecRequest {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "seq 1 50000; printf '__flood_done__\\n'; exec /bin/cat".into(),
            ],
            session_name: "flood".into(),
            ..request.clone()
        };
        drop(agent.terminal_session(flood_request.clone()).await.unwrap());
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut flood = agent.terminal_session(flood_request).await.unwrap();
        until(&mut flood, "__flood_done__").await;
        agent.close_session("flood").await.unwrap();

        // Closing a PTY also ends jobs in the shell's other process groups.
        let mut jobs = agent
            .terminal_session(ExecRequest {
                argv: vec![
                    "/bin/sh".into(),
                    "-ic".into(),
                    r#"sleep 60 & printf '__job__%s__end__\n' "$!"; wait"#.into(),
                ],
                session_name: "jobs".into(),
                ..request.clone()
            })
            .await
            .unwrap();
        let output = until(&mut jobs, "__end__").await;
        let pid: u32 = output
            .split("__job__")
            .nth(1)
            .unwrap()
            .split("__end__")
            .next()
            .unwrap()
            .parse()
            .unwrap();
        agent.close_session("jobs").await.unwrap();
        #[cfg(target_os = "linux")]
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            assert!(
                stat.rsplit_once(')')
                    .unwrap()
                    .1
                    .trim_start()
                    .starts_with('Z'),
                "job survived session close: {stat}"
            );
        }

        // Exiting the shell removes the session and releases its execution slot.
        let mut exiting = agent.terminal_session(request).await.unwrap();
        until(&mut exiting, "__ready__").await;
        exiting
            .input
            .send(ExecInput::Data(b"exit 7\n".to_vec()))
            .await
            .unwrap();
        let exit = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let event = exiting.output.message().await.unwrap().unwrap();
                if let Some(exec_event::Event::Exit(exit)) = event.event {
                    return exit;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(exit.code, 7);
        tokio::time::timeout(Duration::from_secs(3), async {
            while !agent.list_sessions().await.unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        stop_test_agent(task).await;
    }

    #[tokio::test]
    async fn headless_sessions_accept_control_without_taking_over_an_attachment() {
        let box_id = "pbx_t3yzd9y3";
        let ca = generate_context_ca(&derive_context_seed("headless-sessions", "secret")).unwrap();
        let server = issue_certificate(
            &ca,
            &server_subject(box_id).unwrap(),
            CertificatePurpose::Server,
        )
        .unwrap();
        let identity = issue_certificate(
            &ca,
            "pbox.cwd.dev/context/client",
            CertificatePurpose::Client,
        )
        .unwrap();
        let (endpoint, task) = spawn_test_agent(box_id, server, &ca).await;
        let mut client = AgentClient::connect(&endpoint, box_id, &ca.certificate_pem, &identity)
            .await
            .unwrap();
        let request = ExecRequest {
            session_name: "control".into(), user: "root".into(), cwd: "/tmp".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "stty -echo; printf '__ready__\\n'; while IFS= read -r line; do eval \"$line\"; done".into()],
            ..Default::default()
        };
        async fn screen(
            client: &mut AgentClient,
            marker: &str,
        ) -> pbox_proto::agent::ReadSessionResponse {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let screen = client.read_session("control").await.unwrap();
                    if screen.text.contains(marker) {
                        return screen;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("screen did not update")
        }
        client.start_session(request.clone()).await.unwrap();
        assert!(
            !screen(&mut client, "__ready__")
                .await
                .session
                .unwrap()
                .attached
        );
        assert!(client.start_session(request.clone()).await.is_err());
        client
            .send_session(
                "control",
                "printf '__sent__\\n'".into(),
                vec!["Enter".into()],
            )
            .await
            .unwrap();
        assert!(
            !screen(&mut client, "__sent__")
                .await
                .session
                .unwrap()
                .attached
        );
        let attached = client.terminal_session(request).await.unwrap();
        client
            .send_session(
                "control",
                "printf '__attached__\\n'".into(),
                vec!["Enter".into()],
            )
            .await
            .unwrap();
        assert!(
            screen(&mut client, "__attached__")
                .await
                .session
                .unwrap()
                .attached
        );
        assert!(
            client
                .send_session("control", "hello".into(), vec!["unknown".into()])
                .await
                .is_err()
        );
        assert!(
            client
                .send_session("control", "x".repeat(65537), Vec::new())
                .await
                .is_err()
        );
        drop(attached);
        client
            .send_session(
                "control",
                "i=0; while [ $i -lt 500 ]; do printf 'history-%s\\n' \"$i\"; i=$((i+1)); done"
                    .into(),
                vec!["Enter".into()],
            )
            .await
            .unwrap();
        screen(&mut client, "history-499").await;
        let history = client.read_session_history("control").await.unwrap();
        assert!(history.history_supported);
        assert!(history.history.iter().any(|line| line == "history-0"));
        assert!(!history.text.contains("history-0"));
        client.close_session("control").await.unwrap();
        assert!(client.read_session("control").await.is_err());
        assert!(
            client
                .start_session(ExecRequest {
                    session_name: "missing-command".into(),
                    user: "root".into(),
                    argv: vec!["/no/such/pbox-test-command".into()],
                    ..Default::default()
                })
                .await
                .is_err()
        );
        stop_test_agent(task).await;
    }

    #[test]
    fn pty_terminal_defaults_survive_user_switching_and_allow_overrides() {
        for user in ["root", "pbox"] {
            let mut request = ExecRequest {
                argv: vec!["/bin/sh".to_owned()],
                user: user.to_owned(),
                allocate_pty: true,
                ..Default::default()
            };
            let command = super::command_for_request(&request);
            assert!(command.as_std().get_envs().any(|(key, value)| key == "TERM"
                && value == Some(std::ffi::OsStr::new("xterm-256color"))));
            if user == "pbox" {
                assert!(
                    command
                        .as_std()
                        .get_args()
                        .any(|arg| arg.to_string_lossy().starts_with("--preserve-env=")
                            && arg.to_string_lossy().contains("TERM"))
                );
            }
            request.env.insert("TERM".to_owned(), "vt100".to_owned());
            let command = super::command_for_request(&request);
            assert!(
                command
                    .as_std()
                    .get_envs()
                    .any(|(key, value)| key == "TERM"
                        && value == Some(std::ffi::OsStr::new("vt100")))
            );
            request.allocate_pty = false;
            request.env.clear();
            let command = super::command_for_request(&request);
            assert!(!command.as_std().get_envs().any(|(key, _)| key == "TERM"));
        }
    }

    #[tokio::test]
    async fn local_agent_supports_streamed_pty_input_and_output() {
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

        let mut session = agent
            .exec_pty_session(
                vec![
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    r#"printf 'term:%s\n' "$TERM"; read value; printf 'received:%s' "$value""#
                        .to_owned(),
                ],
                "/tmp",
                [],
                "root",
            )
            .await
            .unwrap();
        session
            .input
            .send(ExecInput::Data(b"streamed-input\n".to_vec()))
            .await
            .unwrap();
        session.input.send(ExecInput::Eof).await.unwrap();

        let mut output = Vec::new();
        let mut exited = false;
        while let Some(event) = session.output.message().await.unwrap() {
            match event.event {
                Some(exec_event::Event::Stdout(data)) | Some(exec_event::Event::Stderr(data)) => {
                    output.extend(data)
                }
                Some(exec_event::Event::Exit(exit)) => {
                    assert_eq!(exit.code, 0);
                    exited = true;
                }
                None => panic!("agent returned an empty exec event"),
            }
        }
        assert!(exited);
        assert!(String::from_utf8_lossy(&output).contains("term:xterm-256color"));
        assert!(
            output
                .windows(b"received:streamed-input".len())
                .any(|window| { window == b"received:streamed-input" })
        );

        stop_test_agent(task).await;
    }
    #[tokio::test]
    async fn local_agent_delivers_pty_eof_to_waiting_command() {
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
        let mut session = agent
            .exec_pty_session(vec!["/bin/cat".to_owned()], "/tmp", [], "root")
            .await
            .unwrap();
        session.input.send(ExecInput::Eof).await.unwrap();

        let (output, exit) = tokio::time::timeout(Duration::from_secs(2), async {
            let mut output = Vec::new();
            let mut exit = None;
            while let Some(event) = session.output.message().await.unwrap() {
                match event.event {
                    Some(exec_event::Event::Stdout(data))
                    | Some(exec_event::Event::Stderr(data)) => output.extend(data),
                    Some(exec_event::Event::Exit(status)) => {
                        exit = Some((status.code, status.signal));
                    }
                    None => panic!("agent returned an empty exec event"),
                }
            }
            (output, exit)
        })
        .await
        .expect("PTY command should exit after stdin EOF");

        assert!(output.is_empty());
        assert_eq!(exit, Some((0, 0)));
        stop_test_agent(task).await;
    }

    #[tokio::test]
    async fn forward_tunnels_data_and_closes_after_remote_eof() {
        let target_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target_listener.local_addr().unwrap().port();
        let target_task = tokio::spawn(async move {
            let (mut socket, _) = target_listener.accept().await.unwrap();
            let mut request = Vec::new();
            socket.read_to_end(&mut request).await.unwrap();
            assert_eq!(request, b"ping");
            socket.write_all(b"pong").await.unwrap();
            socket.shutdown().await.unwrap();
        });

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

        let local_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_address = local_listener.local_addr().unwrap();
        let client_task = tokio::spawn(async move {
            let mut socket = TcpStream::connect(local_address).await.unwrap();
            socket.write_all(b"ping").await.unwrap();
            socket.shutdown().await.unwrap();
            let mut response = Vec::new();
            socket.read_to_end(&mut response).await.unwrap();
            response
        });
        let (local_socket, _) = local_listener.accept().await.unwrap();
        agent
            .forward_tcp(local_socket, "127.0.0.1", target_port)
            .await
            .unwrap();

        assert_eq!(client_task.await.unwrap(), b"pong");
        target_task.await.unwrap();
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
    fn non_root_environment_rejects_loader_overrides() {
        assert!(validate_environment_entry("RUST_LOG", "debug", false).is_ok());
        assert!(validate_environment_entry("LD_PRELOAD", "/tmp/hook.so", false).is_err());
        assert!(validate_environment_entry("GCONV_PATH", "/tmp", false).is_err());
        assert!(validate_environment_entry("LD_PRELOAD", "/tmp/hook.so", true).is_ok());
        assert!(validate_environment_entry("BAD-NAME", "value", false).is_err());
    }

    #[test]
    fn safe_mode_strips_special_permission_bits() {
        assert_eq!(safe_mode(0o10755), 0o755);
        assert_eq!(safe_mode(0o100644), 0o644);
    }

    #[tokio::test]
    async fn completed_output_task_cleanup_does_not_poll_handle_twice() {
        await_output_task(tokio::spawn(async {}), EXEC_OUTPUT_DRAIN_TIMEOUT).await;
    }
    #[tokio::test]
    async fn long_running_exec_is_terminated_by_deadline() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 60"]);
        set_process_group(&mut command);
        let mut child = command.spawn().unwrap();
        let (sender, _receiver) = mpsc::channel(1);
        let output_failure = Notify::new();
        let (status, error) = wait_for_child(
            &mut child,
            &sender,
            true,
            &output_failure,
            Some(tokio::time::Instant::now() + Duration::from_millis(25)),
        )
        .await;

        assert_eq!(
            error.expect("deadline should return an error").code(),
            tonic::Code::DeadlineExceeded
        );
        assert!(status.is_ok());
    }
}
