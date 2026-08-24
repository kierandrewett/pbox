use anyhow::{Context, Result};
use clap::Parser;
use pbox_proto::PROTOCOL_VERSION as PROTOCOL;
use pbox_proto::agent::agent_server::{Agent, AgentServer};
use pbox_proto::agent::{
    ExecEvent, ExecExit, ExecRequest, FileChunk, FileResult, ForwardClose, ForwardEvent,
    GetFileRequest, InfoRequest, InfoResponse, PingRequest, PingResponse, exec_event,
    forward_event,
};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status, Streaming};
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
}

#[derive(Clone)]
struct AgentService {
    box_id: String,
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
        request: Request<ExecRequest>,
    ) -> Result<Response<Self::ExecStream>, Status> {
        let request = request.into_inner();
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
        let (sender, receiver) = mpsc::channel(16);
        tokio::spawn(run_command(request, sender));
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn put_file(
        &self,
        request: Request<Streaming<FileChunk>>,
    ) -> Result<Response<FileResult>, Status> {
        let mut stream = request.into_inner();
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("file stream is empty"))?;
        let path = safe_path(&first.path)?;
        let temporary_path = temporary_path(&path);
        let mut file = tokio::fs::File::create(&temporary_path)
            .await
            .map_err(internal_io)?;
        let mut size = 0u64;
        let mut hasher = Sha256::new();
        write_chunk(&mut file, &first, &mut size, &mut hasher).await?;
        while let Some(chunk) = stream.message().await? {
            write_chunk(&mut file, &chunk, &mut size, &mut hasher).await?;
            if chunk.eof {
                break;
            }
        }
        file.flush().await.map_err(internal_io)?;
        drop(file);
        if first.atomic_write {
            tokio::fs::rename(&temporary_path, &path)
                .await
                .map_err(internal_io)?;
        } else {
            tokio::fs::copy(&temporary_path, &path)
                .await
                .map_err(internal_io)?;
            let _ = tokio::fs::remove_file(&temporary_path).await;
        }
        if first.mode != 0 {
            let mode = first.mode;
            tokio::task::spawn_blocking(move || set_mode(&path, mode))
                .await
                .map_err(|error| Status::internal(error.to_string()))??;
        }
        Ok(Response::new(FileResult {
            size,
            digest: hex_digest(hasher.finalize()),
        }))
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
        let path = safe_path(&request.path)?;
        let (sender, receiver) = mpsc::channel(16);
        tokio::spawn(read_file(path, sender));
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn forward(
        &self,
        request: Request<Streaming<ForwardEvent>>,
    ) -> Result<Response<Self::ForwardStream>, Status> {
        let mut inbound = request.into_inner();
        let open_event = inbound
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("forward stream must start with open"))?;
        let forward_event::Event::Open(open) = open_event
            .event
            .ok_or_else(|| Status::invalid_argument("forward stream missing open event"))?
        else {
            return Err(Status::invalid_argument(
                "forward stream must start with open",
            ));
        };
        if open.protocol_version != PROTOCOL
            || open.host.is_empty()
            || open.port == 0
            || open.port > u16::MAX as u32
        {
            return Err(Status::invalid_argument("invalid forward target"));
        }
        let socket = TcpStream::connect((open.host.as_str(), open.port as u16))
            .await
            .map_err(|error| Status::unavailable(format!("connect forward target: {error}")))?;
        let (mut reader, mut writer) = socket.into_split();
        let (sender, receiver) = mpsc::channel(32);
        tokio::spawn(async move {
            let mut buffer = [0u8; 8192];
            loop {
                tokio::select! {
                    message = inbound.message() => {
                        match message {
                            Ok(Some(event)) => match event.event {
                                Some(forward_event::Event::Data(data)) => {
                                    if writer.write_all(&data).await.is_err() {
                                        break;
                                    }
                                }
                                Some(forward_event::Event::Close(_)) | None => break,
                                Some(forward_event::Event::Open(_)) => {}
                            },
                            _ => break,
                        }
                    }
                    result = reader.read(&mut buffer) => {
                        match result {
                            Ok(0) => break,
                            Ok(size) => {
                                if sender
                                    .send(Ok(ForwardEvent {
                                        event: Some(forward_event::Event::Data(
                                            buffer[..size].to_vec(),
                                        )),
                                    }))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
            let _ = sender
                .send(Ok(ForwardEvent {
                    event: Some(forward_event::Event::Close(ForwardClose { code: 0 })),
                }))
                .await;
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

async fn run_command(request: ExecRequest, sender: mpsc::Sender<Result<ExecEvent, Status>>) {
    let mut command = if request.user.is_empty() || request.user == "root" {
        let mut command = Command::new(&request.argv[0]);
        command.args(&request.argv[1..]);
        command
    } else {
        let mut command = Command::new("sudo");
        command
            .arg("-n")
            .arg("-u")
            .arg(&request.user)
            .arg("--")
            .arg(&request.argv[0]);
        command.args(&request.argv[1..]);
        command
    };
    if !request.cwd.is_empty() {
        command.current_dir(request.cwd);
    }
    command.envs(request.env);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = sender
                .send(Err(Status::internal(format!("spawn command: {error}"))))
                .await;
            return;
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = tokio::spawn(send_output(stdout, sender.clone(), true));
    let stderr_task = tokio::spawn(send_output(stderr, sender.clone(), false));
    let status = child.wait().await;
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    match status {
        Ok(status) => {
            let _ = sender
                .send(Ok(ExecEvent {
                    event: Some(exec_event::Event::Exit(ExecExit {
                        code: status.code().unwrap_or(-1),
                        signal: 0,
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

async fn send_output<R>(
    reader: Option<R>,
    sender: mpsc::Sender<Result<ExecEvent, Status>>,
    stdout: bool,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(mut reader) = reader else { return };
    let mut buffer = [0u8; 8192];
    loop {
        let size = match reader.read(&mut buffer).await {
            Ok(0) => return,
            Ok(size) => size,
            Err(error) => {
                let _ = sender.send(Err(Status::internal(error.to_string()))).await;
                return;
            }
        };
        let event = if stdout {
            exec_event::Event::Stdout(buffer[..size].to_vec())
        } else {
            exec_event::Event::Stderr(buffer[..size].to_vec())
        };
        if sender
            .send(Ok(ExecEvent { event: Some(event) }))
            .await
            .is_err()
        {
            return;
        }
    }
}

async fn write_chunk(
    file: &mut tokio::fs::File,
    chunk: &FileChunk,
    size: &mut u64,
    hasher: &mut Sha256,
) -> Result<(), Status> {
    file.write_all(&chunk.data).await.map_err(internal_io)?;
    *size += chunk.data.len() as u64;
    hasher.update(&chunk.data);
    Ok(())
}

async fn read_file(path: PathBuf, sender: mpsc::Sender<Result<FileChunk, Status>>) {
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) => {
            let _ = sender.send(Err(internal_io(error))).await;
            return;
        }
    };
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(error) => {
            let _ = sender.send(Err(internal_io(error))).await;
            return;
        }
    };
    let mut buffer = [0u8; 8192];
    let mut first = true;
    loop {
        match file.read(&mut buffer).await {
            Ok(0) => {
                let _ = sender
                    .send(Ok(FileChunk {
                        path: path.to_string_lossy().into_owned(),
                        mode: metadata_mode(&metadata),
                        eof: true,
                        ..Default::default()
                    }))
                    .await;
                return;
            }
            Ok(size) => {
                let chunk = FileChunk {
                    path: if first {
                        path.to_string_lossy().into_owned()
                    } else {
                        String::new()
                    },
                    mode: if first { metadata_mode(&metadata) } else { 0 },
                    data: buffer[..size].to_vec(),
                    eof: false,
                    ..Default::default()
                };
                first = false;
                if sender.send(Ok(chunk)).await.is_err() {
                    return;
                }
            }
            Err(error) => {
                let _ = sender.send(Err(internal_io(error))).await;
                return;
            }
        }
    }
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let certificate = tokio::fs::read_to_string(&args.certificate)
        .await
        .with_context(|| format!("read certificate {}", args.certificate.display()))?;
    let private_key = tokio::fs::read_to_string(&args.private_key)
        .await
        .with_context(|| format!("read private key {}", args.private_key.display()))?;
    let client_ca = tokio::fs::read_to_string(&args.client_ca)
        .await
        .with_context(|| format!("read client CA {}", args.client_ca.display()))?;
    let identity = Identity::from_pem(certificate, private_key);
    let tls = ServerTlsConfig::new()
        .identity(identity)
        .client_ca_root(Certificate::from_pem(client_ca));
    let address = args.listen.parse().context("parse agent listen address")?;
    println!("pbox-agent listening on {}", args.listen);
    Server::builder()
        .tls_config(tls)
        .context("configure agent TLS")?
        .add_service(AgentServer::new(AgentService {
            box_id: args.box_id,
        }))
        .serve(address)
        .await
        .context("run pbox-agent server")?;
    Ok(())
}
