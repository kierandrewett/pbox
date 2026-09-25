use super::{AgentClientError, GeneratedAgentClient};
use pbox_proto::PROTOCOL_VERSION;
use pbox_proto::agent::{ForwardClose, ForwardOpen, ReverseForwardEvent, reverse_forward_event};
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Streaming, transport::Channel};

type Event = reverse_forward_event::Event;

pub struct ReverseForwardSession {
    pub guest_address: String,
    target_host: String,
    target_port: u16,
    outbound: mpsc::Sender<ReverseForwardEvent>,
    inbound: Streaming<ReverseForwardEvent>,
}

pub(super) async fn start(
    client: &mut GeneratedAgentClient<Channel>,
    listen_host: String,
    listen_port: u16,
    target_host: String,
    target_port: u16,
) -> Result<ReverseForwardSession, AgentClientError> {
    super::validate_forward_target(&listen_host, listen_port)?;
    super::validate_forward_target(&target_host, target_port)?;
    let (outbound, receiver) = mpsc::channel(32);
    outbound
        .send(ReverseForwardEvent {
            connection_id: 0,
            event: Some(Event::Listen(ForwardOpen {
                protocol_version: PROTOCOL_VERSION,
                host: listen_host,
                port: listen_port as u32,
            })),
        })
        .await
        .map_err(|_| AgentClientError::ForwardRequestClosed)?;
    let mut inbound = client
        .reverse_forward(ReceiverStream::new(receiver))
        .await?
        .into_inner();
    let first = inbound.message().await?.ok_or_else(|| {
        AgentClientError::Rpc(tonic::Status::unknown("guest listener closed before ready"))
    })?;
    let Some(Event::Ready(guest_address)) = first.event else {
        return Err(AgentClientError::Rpc(tonic::Status::invalid_argument(
            "guest listener did not report ready",
        )));
    };
    Ok(ReverseForwardSession {
        guest_address,
        target_host,
        target_port,
        outbound,
        inbound,
    })
}

impl ReverseForwardSession {
    pub async fn run(mut self) -> Result<(), AgentClientError> {
        let mut connections: HashMap<u64, mpsc::Sender<Event>> = HashMap::new();
        while let Some(message) = self.inbound.message().await? {
            let id = message.connection_id;
            match message.event {
                Some(Event::Connected(true)) if id != 0 => {
                    let (sender, input) = mpsc::channel(32);
                    connections.insert(id, sender);
                    tokio::spawn(bridge(
                        id,
                        self.target_host.clone(),
                        self.target_port,
                        input,
                        self.outbound.clone(),
                    ));
                }
                Some(event @ (Event::Data(_) | Event::Close(_))) => {
                    if let Some(sender) = connections.get(&id) {
                        let full_close = matches!(&event, Event::Close(close) if !close.half_close);
                        if sender.send(event).await.is_err() || full_close {
                            connections.remove(&id);
                        }
                    }
                }
                _ => {
                    return Err(AgentClientError::Rpc(tonic::Status::invalid_argument(
                        "invalid reverse forward event",
                    )));
                }
            }
            connections.retain(|_, sender| !sender.is_closed());
        }
        Err(AgentClientError::Rpc(tonic::Status::unavailable(
            "guest listener disconnected",
        )))
    }
}

async fn bridge(
    id: u64,
    host: String,
    port: u16,
    mut input: mpsc::Receiver<Event>,
    outbound: mpsc::Sender<ReverseForwardEvent>,
) {
    let socket = match tokio::time::timeout(
        super::AGENT_CONNECT_TIMEOUT,
        TcpStream::connect((host.as_str(), port)),
    )
    .await
    {
        Ok(Ok(socket)) => socket,
        _ => {
            let _ = send(
                &outbound,
                id,
                Event::Close(ForwardClose {
                    code: 1,
                    half_close: false,
                }),
            )
            .await;
            return;
        }
    };
    let (mut reader, mut writer) = socket.into_split();
    let mut buffer = [0u8; 8192];
    let mut read_closed = false;
    let mut write_closed = false;
    loop {
        if read_closed && write_closed {
            break;
        }
        tokio::select! {
            read = reader.read(&mut buffer), if !read_closed => {
                match read {
                    Ok(0) => {
                        read_closed = true;
                        if send(&outbound, id, Event::Close(ForwardClose { code: 0, half_close: true })).await.is_err() { break; }
                    }
                    Ok(size) => if send(&outbound, id, Event::Data(buffer[..size].to_vec())).await.is_err() { break; },
                    Err(_) => break,
                }
            }
            event = input.recv(), if !write_closed => {
                match event {
                    Some(Event::Data(data)) => if writer.write_all(&data).await.is_err() { break; },
                    Some(Event::Close(close)) if close.code == 0 && close.half_close => {
                        if writer.shutdown().await.is_err() { break; }
                        write_closed = true;
                    }
                    _ => break,
                }
            }
        }
    }
    let _ = send(
        &outbound,
        id,
        Event::Close(ForwardClose {
            code: 0,
            half_close: false,
        }),
    )
    .await;
}

async fn send(
    outbound: &mpsc::Sender<ReverseForwardEvent>,
    id: u64,
    event: Event,
) -> Result<(), mpsc::error::SendError<ReverseForwardEvent>> {
    outbound
        .send(ReverseForwardEvent {
            connection_id: id,
            event: Some(event),
        })
        .await
}
