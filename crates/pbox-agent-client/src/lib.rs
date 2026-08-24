use pbox_crypto::{CertificateMaterial, server_dns_name};
use pbox_proto::PROTOCOL_VERSION;
use pbox_proto::agent::{
    ExecEvent, ExecRequest, FileChunk, FileResult, ForwardEvent, GetFileRequest, InfoRequest,
    InfoResponse, PingRequest, PingResponse, agent_client::AgentClient as GeneratedAgentClient,
    exec_event,
};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};
use tonic::{Status, Streaming};
const MAX_COLLECTED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum AgentClientError {
    #[error("invalid agent endpoint: {0}")]
    Endpoint(String),
    #[error("agent transport failed: {0}")]
    Transport(#[from] tonic::transport::Error),
    #[error("agent RPC failed: {0}")]
    Rpc(#[from] Status),
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
        let domain = server_dns_name(box_id)
            .map_err(|error| AgentClientError::Identity(error.to_string()))?;
        let endpoint = Endpoint::from_shared(endpoint.to_owned())
            .map_err(|error| AgentClientError::Endpoint(error.to_string()))?
            .tls_config(
                ClientTlsConfig::new()
                    .domain_name(domain)
                    .ca_certificate(Certificate::from_pem(ca_pem))
                    .identity(Identity::from_pem(
                        client_identity.certificate_pem.clone(),
                        client_identity.private_key_pem.clone(),
                    )),
            )?;
        let inner = GeneratedAgentClient::connect(endpoint).await?;
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
        collect_exec_stream(stream).await
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
        Ok(self
            .inner
            .put_file(tokio_stream::iter(chunks))
            .await?
            .into_inner())
    }

    pub async fn get_file(&mut self, path: impl Into<String>) -> Result<Vec<u8>, AgentClientError> {
        let mut stream = self
            .inner
            .get_file(GetFileRequest {
                protocol_version: PROTOCOL_VERSION,
                path: path.into(),
            })
            .await?
            .into_inner();
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
