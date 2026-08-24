use pbox_crypto::{CertificateMaterial, server_dns_name};
use pbox_proto::PROTOCOL_VERSION;
use pbox_proto::agent::{
    ExecRequest, FileChunk, FileResult, GetFileRequest, InfoRequest, InfoResponse, PingRequest,
    PingResponse, agent_client::AgentClient as GeneratedAgentClient, exec_event,
};
use tonic::Status;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

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
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExecResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: i32,
    pub signal: i32,
}

pub struct AgentClient {
    inner: GeneratedAgentClient<Channel>,
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
        Ok(Self { inner })
    }

    pub async fn info(&mut self) -> Result<InfoResponse, AgentClientError> {
        Ok(self.inner.info(InfoRequest {}).await?.into_inner())
    }

    pub async fn ping(&mut self) -> Result<PingResponse, AgentClientError> {
        Ok(self
            .inner
            .ping(PingRequest {
                protocol_version: PROTOCOL_VERSION,
            })
            .await?
            .into_inner())
    }

    pub async fn exec(
        &mut self,
        argv: Vec<String>,
        cwd: impl Into<String>,
        env: impl IntoIterator<Item = (String, String)>,
        user: impl Into<String>,
    ) -> Result<ExecResult, AgentClientError> {
        let mut stream = self
            .inner
            .exec(ExecRequest {
                protocol_version: PROTOCOL_VERSION,
                argv,
                cwd: cwd.into(),
                env: env.into_iter().collect(),
                user: user.into(),
                allocate_pty: false,
            })
            .await?
            .into_inner();
        let mut result = ExecResult::default();
        while let Some(event) = stream.message().await? {
            match event.event {
                Some(exec_event::Event::Stdout(data)) => result.stdout.extend(data),
                Some(exec_event::Event::Stderr(data)) => result.stderr.extend(data),
                Some(exec_event::Event::Exit(exit)) => {
                    result.code = exit.code;
                    result.signal = exit.signal;
                }
                None => {}
            }
        }
        Ok(result)
    }

    pub async fn put_file(
        &mut self,
        path: impl Into<String>,
        data: Vec<u8>,
        mode: u32,
        atomic_write: bool,
    ) -> Result<FileResult, AgentClientError> {
        let path = path.into();
        let mut chunks = Vec::new();
        if data.is_empty() {
            chunks.push(FileChunk {
                path,
                mode,
                atomic_write,
                eof: true,
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
                    ..Default::default()
                });
            }
            chunks.push(FileChunk {
                eof: true,
                ..Default::default()
            });
        }
        let stream = tokio_stream::iter(chunks);
        Ok(self.inner.put_file(stream).await?.into_inner())
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
        let mut data = Vec::new();
        while let Some(chunk) = stream.message().await? {
            data.extend(chunk.data);
            if chunk.eof {
                break;
            }
        }
        Ok(data)
    }
}
