mod transport;

use hyper_util::rt::TokioIo;
use pbox_crypto::{CertificateMaterial, server_dns_name};
use pbox_proto::PROTOCOL_VERSION;
pub use pbox_proto::agent::{ExecEvent, exec_event};
use pbox_proto::agent::{
    ExecRequest, FileChunk, FileResult, ForwardClose, ForwardEvent, ForwardOpen, GetFileRequest,
    InfoRequest, InfoResponse, PingRequest, PingResponse,
    agent_client::AgentClient as GeneratedAgentClient, forward_event,
};
use rustls::ClientConfig;
use rustls::RootCertStore;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsConnector;
use tokio_stream::wrappers::ReceiverStream;
use tonic::codegen::{Service, http::Uri};
use tonic::transport::{Channel, Endpoint};
use tonic::{Status, Streaming};
use transport::AgentStream;
const AGENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const AGENT_RPC_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_COLLECTED_BYTES: usize = 64 * 1024 * 1024;
const EXEC_INPUT_KEEPALIVE: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum AgentClientError {
    #[error("invalid agent endpoint: {0}")]
    Endpoint(String),
    #[error("agent transport failed: {0}")]
    Transport(#[from] tonic::transport::Error),
    #[error("local forwarding I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("agent exec request stream closed")]
    ExecRequestClosed,
    #[error("agent forward request stream closed")]
    ForwardRequestClosed,
    #[error("agent RPC failed: {0}")]
    Rpc(#[from] Status),
    #[error("agent TLS configuration failed: {0}")]
    Tls(String),
    #[error("agent operation timed out after 600 seconds")]
    Timeout,
    #[error("invalid box identity: {0}")]
    Identity(String),
    #[error("agent protocol mismatch: expected {expected}, got {actual}")]
    ProtocolMismatch { expected: u32, actual: u32 },
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExecResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: i32,
    pub signal: i32,
    pub exited: bool,
}

#[derive(Debug)]
pub enum ExecInput {
    Data(Vec<u8>),
    Eof,
}

pub struct ExecPtySession {
    pub input: mpsc::Sender<ExecInput>,
    pub output: Streaming<ExecEvent>,
}

#[derive(Clone)]
struct AgentTlsConnector {
    tls: TlsConnector,
    domain: String,
    ssh_host: Option<String>,
}

impl AgentTlsConnector {
    fn new(
        domain: String,
        ca_pem: &str,
        client_identity: &CertificateMaterial,
    ) -> Result<Self, AgentClientError> {
        let mut roots = RootCertStore::empty();
        for certificate in pem::parse_many(ca_pem)
            .map_err(|error| AgentClientError::Tls(format!("parse CA PEM: {error}")))?
        {
            if certificate.tag() != "CERTIFICATE" {
                continue;
            }
            roots
                .add(CertificateDer::from(certificate.contents().to_vec()))
                .map_err(|error| AgentClientError::Tls(format!("add CA certificate: {error}")))?;
        }
        if roots.is_empty() {
            return Err(AgentClientError::Tls(
                "CA PEM contains no certificates".to_owned(),
            ));
        }

        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            client_identity.private_key_der.clone(),
        ));
        let mut config = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_root_certificates(roots)
            .with_client_auth_cert(
                vec![CertificateDer::from(
                    client_identity.certificate_der.clone(),
                )],
                key,
            )
            .map_err(|error| AgentClientError::Tls(error.to_string()))?;
        config.alpn_protocols = vec![b"h2".to_vec()];
        Ok(Self {
            tls: TlsConnector::from(Arc::new(config)),
            domain,
            ssh_host: None,
        })
    }
}

impl Service<Uri> for AgentTlsConnector {
    type Response = TokioIo<tokio_rustls::client::TlsStream<AgentStream>>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let tls = self.tls.clone();
        let domain = self.domain.clone();
        let ssh_host = self.ssh_host.clone();
        Box::pin(async move {
            let authority = uri
                .authority()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "agent endpoint has no authority",
                    )
                })?
                .as_str()
                .to_owned();
            let stream = AgentStream::connect(&authority, ssh_host.as_deref()).await?;
            let server_name = ServerName::try_from(domain)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
            tls.connect(server_name, stream)
                .await
                .map(TokioIo::new)
                .map_err(|error| io::Error::other(format!("TLS handshake failed: {error}")))
        })
    }
}

pub struct AgentClient {
    inner: GeneratedAgentClient<Channel>,
    expected_box_id: String,
}

impl AgentClient {
    pub async fn connect(
        endpoint: &str,
        box_id: &str,
        ca_pem: &str,
        client_identity: &CertificateMaterial,
    ) -> Result<Self, AgentClientError> {
        Self::connect_via_ssh(endpoint, box_id, ca_pem, client_identity, None).await
    }

    /// Connect through an optional SSH host while retaining guest TLS identity checks.
    pub async fn connect_via_ssh(
        endpoint: &str,
        box_id: &str,
        ca_pem: &str,
        client_identity: &CertificateMaterial,
        ssh_host: Option<&str>,
    ) -> Result<Self, AgentClientError> {
        let domain = server_dns_name(box_id)
            .map_err(|error| AgentClientError::Identity(error.to_string()))?;
        let endpoint = Endpoint::from_shared(endpoint.to_owned())
            .map_err(|error| AgentClientError::Endpoint(error.to_string()))?
            .connect_timeout(AGENT_CONNECT_TIMEOUT)
            .timeout(AGENT_RPC_TIMEOUT);
        let connector_endpoint = Endpoint::from_shared(
            endpoint
                .uri()
                .to_string()
                .replacen("https://", "http://", 1),
        )
        .map_err(|error| AgentClientError::Endpoint(error.to_string()))?
        .connect_timeout(AGENT_CONNECT_TIMEOUT)
        .timeout(AGENT_RPC_TIMEOUT);
        validate_https_endpoint(&endpoint)?;
        let mut connector = AgentTlsConnector::new(domain, ca_pem, client_identity)?;
        connector.ssh_host = ssh_host.map(str::to_owned);
        let channel = connector_endpoint.connect_with_connector(connector).await?;
        let inner = GeneratedAgentClient::new(channel);
        Ok(Self {
            inner,
            expected_box_id: box_id.to_owned(),
        })
    }
    pub async fn info(&mut self) -> Result<InfoResponse, AgentClientError> {
        let response = self.inner.info(InfoRequest {}).await?.into_inner();
        self.validate_identity(response.protocol_version, &response.box_id)?;
        Ok(response)
    }

    pub async fn ping(&mut self) -> Result<PingResponse, AgentClientError> {
        let response = self
            .inner
            .ping(PingRequest {
                protocol_version: PROTOCOL_VERSION,
            })
            .await?
            .into_inner();
        self.validate_identity(response.protocol_version, &response.box_id)?;
        Ok(response)
    }

    pub async fn exec(
        &mut self,
        argv: Vec<String>,
        cwd: impl Into<String>,
        env: impl IntoIterator<Item = (String, String)>,
        user: impl Into<String>,
    ) -> Result<ExecResult, AgentClientError> {
        self.exec_with_mode(argv, cwd, env, user, false, Vec::new())
            .await
    }

    pub async fn exec_pty(
        &mut self,
        argv: Vec<String>,
        cwd: impl Into<String>,
        env: impl IntoIterator<Item = (String, String)>,
        user: impl Into<String>,
        stdin: Vec<u8>,
    ) -> Result<ExecResult, AgentClientError> {
        self.exec_with_mode(argv, cwd, env, user, true, stdin).await
    }

    pub async fn exec_stream<S>(
        &mut self,
        requests: S,
    ) -> Result<Streaming<ExecEvent>, AgentClientError>
    where
        S: tonic::IntoStreamingRequest<Message = ExecRequest>,
    {
        Ok(self.inner.exec(requests).await?.into_inner())
    }

    pub async fn exec_pty_session(
        &mut self,
        argv: Vec<String>,
        cwd: impl Into<String>,
        env: impl IntoIterator<Item = (String, String)>,
        user: impl Into<String>,
    ) -> Result<ExecPtySession, AgentClientError> {
        let (request_sender, request_receiver) = mpsc::channel(32);
        request_sender
            .send(ExecRequest {
                protocol_version: PROTOCOL_VERSION,
                argv,
                cwd: cwd.into(),
                env: env.into_iter().collect(),
                user: user.into(),
                allocate_pty: true,
                stdin_eof: false,
                ..Default::default()
            })
            .await
            .map_err(|_| AgentClientError::ExecRequestClosed)?;
        let output = self
            .inner
            .exec(ReceiverStream::new(request_receiver))
            .await?
            .into_inner();
        let (input_sender, mut input_receiver) = mpsc::channel(32);
        tokio::spawn(async move {
            let mut sent_eof = false;
            let mut keepalive = tokio::time::interval(EXEC_INPUT_KEEPALIVE);
            keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            keepalive.tick().await;
            loop {
                tokio::select! {
                    input = input_receiver.recv() => {
                        let Some(input) = input else {
                            break;
                        };
                        let (stdin, stdin_eof) = match input {
                            ExecInput::Data(data) => (data, false),
                            ExecInput::Eof => (Vec::new(), true),
                        };
                        if request_sender
                            .send(ExecRequest {
                                protocol_version: PROTOCOL_VERSION,
                                stdin,
                                stdin_eof,
                                ..Default::default()
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if stdin_eof {
                            sent_eof = true;
                            break;
                        }
                    }
                    _ = keepalive.tick() => {
                        if request_sender
                            .send(ExecRequest {
                                protocol_version: PROTOCOL_VERSION,
                                ..Default::default()
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            if !sent_eof {
                let _ = request_sender
                    .send(ExecRequest {
                        protocol_version: PROTOCOL_VERSION,
                        stdin_eof: true,
                        ..Default::default()
                    })
                    .await;
            }
        });
        Ok(ExecPtySession {
            input: input_sender,
            output,
        })
    }

    pub async fn exec_with_mode(
        &mut self,
        argv: Vec<String>,
        cwd: impl Into<String>,
        env: impl IntoIterator<Item = (String, String)>,
        user: impl Into<String>,
        allocate_pty: bool,
        stdin: Vec<u8>,
    ) -> Result<ExecResult, AgentClientError> {
        let request = ExecRequest {
            protocol_version: PROTOCOL_VERSION,
            argv,
            cwd: cwd.into(),
            env: env.into_iter().collect(),
            user: user.into(),
            allocate_pty,
            stdin,
            stdin_eof: true,
        };
        let stream = self.exec_stream(tokio_stream::iter([request])).await?;
        tokio::time::timeout(AGENT_RPC_TIMEOUT, collect_exec_stream(stream))
            .await
            .map_err(|_| AgentClientError::Timeout)?
    }

    pub async fn forward_stream<S>(
        &mut self,
        requests: S,
    ) -> Result<Streaming<ForwardEvent>, AgentClientError>
    where
        S: tonic::IntoStreamingRequest<Message = ForwardEvent>,
    {
        Ok(self.inner.forward(requests).await?.into_inner())
    }
    pub async fn forward_tcp(
        &mut self,
        socket: TcpStream,
        host: impl Into<String>,
        port: u16,
    ) -> Result<(), AgentClientError> {
        let host = host.into();
        validate_forward_target(&host, port)?;
        let (sender, receiver) = mpsc::channel(32);
        sender
            .send(ForwardEvent {
                event: Some(forward_event::Event::Open(ForwardOpen {
                    protocol_version: PROTOCOL_VERSION,
                    host,
                    port: port as u32,
                })),
            })
            .await
            .map_err(|_| AgentClientError::ForwardRequestClosed)?;
        let mut stream = self
            .inner
            .forward(ReceiverStream::new(receiver))
            .await?
            .into_inner();
        let (mut reader, mut writer) = socket.into_split();
        let mut buffer = [0u8; 8192];
        let mut local_write_closed = false;
        let mut remote_write_closed = false;
        loop {
            if local_write_closed && remote_write_closed {
                let _ = sender
                    .send(ForwardEvent {
                        event: Some(forward_event::Event::Close(ForwardClose {
                            code: 0,
                            half_close: false,
                        })),
                    })
                    .await;
                return Ok(());
            }
            tokio::select! {
                result = reader.read(&mut buffer), if !local_write_closed => {
                    let size = result?;
                    if size == 0 {
                        sender
                            .send(ForwardEvent {
                                event: Some(forward_event::Event::Close(ForwardClose {
                                    code: 0,
                                    half_close: true,
                                })),
                            })
                            .await
                            .map_err(|_| AgentClientError::ForwardRequestClosed)?;
                        local_write_closed = true;
                    } else {
                        sender
                            .send(ForwardEvent {
                                event: Some(forward_event::Event::Data(buffer[..size].to_vec())),
                            })
                            .await
                            .map_err(|_| AgentClientError::ForwardRequestClosed)?;
                    }
                }
                result = stream.message(), if !remote_write_closed => {
                    let event = result?
                        .ok_or_else(|| AgentClientError::Rpc(Status::unknown(
                            "agent forward stream ended without close",
                        )))?;
                    match event.event {
                        Some(forward_event::Event::Data(data)) => {
                            writer.write_all(&data).await?;
                        }
                        Some(forward_event::Event::Close(close)) => {
                            writer.shutdown().await?;
                            if close.code != 0 {
                                return Err(AgentClientError::Rpc(Status::unknown(format!(
                                    "agent forward closed with code {}",
                                    close.code
                                ))));
                            }
                            if close.half_close {
                                remote_write_closed = true;
                            } else {
                                return Ok(());
                            }
                        }
                        Some(forward_event::Event::Open(_)) | None => {
                            return Err(AgentClientError::Rpc(Status::invalid_argument(
                                "agent forward stream sent an invalid event",
                            )));
                        }
                    }
                }
            }
        }
    }

    pub async fn put_file(
        &mut self,
        path: impl Into<String>,
        data: Vec<u8>,
        mode: u32,
        atomic_write: bool,
    ) -> Result<FileResult, AgentClientError> {
        if data.len() > MAX_COLLECTED_BYTES {
            return Err(AgentClientError::Rpc(Status::resource_exhausted(
                "upload exceeds the 64 MiB collection limit",
            )));
        }
        let path = path.into();
        let mut chunks = Vec::new();
        if data.is_empty() {
            chunks.push(FileChunk {
                path: path.clone(),
                mode,
                atomic_write,
                eof: true,
                protocol_version: PROTOCOL_VERSION,
                ..Default::default()
            });
        } else {
            for (index, chunk) in data.chunks(8192).enumerate() {
                chunks.push(FileChunk {
                    path: if index == 0 {
                        path.clone()
                    } else {
                        String::new()
                    },
                    mode: if index == 0 { mode } else { 0 },
                    atomic_write: index == 0 && atomic_write,
                    data: chunk.to_vec(),
                    eof: false,
                    protocol_version: if index == 0 { PROTOCOL_VERSION } else { 0 },
                    ..Default::default()
                });
            }
            chunks.push(FileChunk {
                eof: true,
                ..Default::default()
            });
        }
        let response = tokio::time::timeout(
            AGENT_RPC_TIMEOUT,
            self.inner.put_file(tokio_stream::iter(chunks)),
        )
        .await
        .map_err(|_| AgentClientError::Timeout)??;
        Ok(response.into_inner())
    }

    pub async fn get_file(&mut self, path: impl Into<String>) -> Result<Vec<u8>, AgentClientError> {
        let stream = self
            .inner
            .get_file(GetFileRequest {
                protocol_version: PROTOCOL_VERSION,
                path: path.into(),
            })
            .await?
            .into_inner();
        tokio::time::timeout(AGENT_RPC_TIMEOUT, collect_file_stream(stream))
            .await
            .map_err(|_| AgentClientError::Timeout)?
    }

    fn validate_identity(
        &self,
        protocol_version: u32,
        box_id: &str,
    ) -> Result<(), AgentClientError> {
        if protocol_version != PROTOCOL_VERSION {
            return Err(AgentClientError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                actual: protocol_version,
            });
        }
        if box_id != self.expected_box_id {
            return Err(AgentClientError::Identity(format!(
                "expected {}, got {box_id}",
                self.expected_box_id
            )));
        }
        Ok(())
    }
}

async fn collect_exec_stream(
    mut stream: Streaming<ExecEvent>,
) -> Result<ExecResult, AgentClientError> {
    let mut result = ExecResult::default();
    while let Some(event) = stream.message().await? {
        if result.exited {
            return Err(AgentClientError::Rpc(Status::invalid_argument(
                "agent exec stream sent data after exit",
            )));
        }
        match event.event {
            Some(exec_event::Event::Stdout(data)) => {
                if result.stdout.len().saturating_add(data.len()) > MAX_COLLECTED_BYTES {
                    return Err(AgentClientError::Rpc(Status::resource_exhausted(
                        "stdout exceeds the 64 MiB collection limit",
                    )));
                }
                result.stdout.extend(data);
            }
            Some(exec_event::Event::Stderr(data)) => {
                if result.stderr.len().saturating_add(data.len()) > MAX_COLLECTED_BYTES {
                    return Err(AgentClientError::Rpc(Status::resource_exhausted(
                        "stderr exceeds the 64 MiB collection limit",
                    )));
                }
                result.stderr.extend(data);
            }
            Some(exec_event::Event::Exit(exit)) => {
                result.code = exit.code;
                result.signal = exit.signal;
                result.exited = true;
            }
            None => {
                return Err(AgentClientError::Rpc(Status::invalid_argument(
                    "agent exec stream sent an empty event",
                )));
            }
        }
    }
    if !result.exited {
        return Err(AgentClientError::Rpc(Status::unknown(
            "agent exec stream ended without exit status",
        )));
    }
    Ok(result)
}
async fn collect_file_stream(
    mut stream: Streaming<FileChunk>,
) -> Result<Vec<u8>, AgentClientError> {
    let mut complete = false;
    let mut first = true;
    let mut data = Vec::new();
    while let Some(chunk) = stream.message().await? {
        if complete {
            return Err(AgentClientError::Rpc(Status::invalid_argument(
                "agent file stream sent data after EOF",
            )));
        }
        if first {
            if chunk.protocol_version != PROTOCOL_VERSION {
                return Err(AgentClientError::ProtocolMismatch {
                    expected: PROTOCOL_VERSION,
                    actual: chunk.protocol_version,
                });
            }
            first = false;
        } else if chunk.protocol_version != 0 {
            return Err(AgentClientError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                actual: chunk.protocol_version,
            });
        }
        if data.len().saturating_add(chunk.data.len()) > MAX_COLLECTED_BYTES {
            return Err(AgentClientError::Rpc(Status::resource_exhausted(
                "download exceeds the 64 MiB collection limit",
            )));
        }
        data.extend(chunk.data);
        if chunk.eof {
            complete = true;
        }
    }
    if !complete {
        return Err(AgentClientError::Rpc(Status::unknown(
            "agent file stream ended without EOF marker",
        )));
    }
    Ok(data)
}
fn validate_forward_target(host: &str, port: u16) -> Result<(), AgentClientError> {
    if host.is_empty() {
        return Err(AgentClientError::Endpoint(
            "agent forward target host cannot be empty".to_owned(),
        ));
    }
    if host.contains('\0') {
        return Err(AgentClientError::Endpoint(
            "agent forward target host cannot contain NUL bytes".to_owned(),
        ));
    }
    if port == 0 {
        return Err(AgentClientError::Endpoint(
            "agent forward target port cannot be zero".to_owned(),
        ));
    }
    Ok(())
}

fn validate_https_endpoint(endpoint: &Endpoint) -> Result<(), AgentClientError> {
    if endpoint
        .uri()
        .scheme_str()
        .is_none_or(|scheme| !scheme.eq_ignore_ascii_case("https"))
    {
        return Err(AgentClientError::Endpoint(
            "agent endpoint must use https".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_https_agent_endpoints() {
        let endpoint = Endpoint::from_shared("http://127.0.0.1:7443").unwrap();
        let error = validate_https_endpoint(&endpoint).unwrap_err();
        assert!(matches!(
            error,
            AgentClientError::Endpoint(message) if message == "agent endpoint must use https"
        ));
    }
    #[test]
    fn forward_target_validation_rejects_empty_host_and_zero_port() {
        assert!(matches!(
            validate_forward_target("", 8080),
            Err(AgentClientError::Endpoint(message))
                if message == "agent forward target host cannot be empty"
        ));
        assert!(matches!(
            validate_forward_target("127.0.0.1", 0),
            Err(AgentClientError::Endpoint(message))
                if message == "agent forward target port cannot be zero"
        ));
        assert!(validate_forward_target("127.0.0.1", 8080).is_ok());
    }
}
