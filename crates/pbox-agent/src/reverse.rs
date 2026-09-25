use pbox_proto::PROTOCOL_VERSION;
use pbox_proto::agent::{ForwardClose, ReverseForwardEvent, reverse_forward_event};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

pub(crate) type EventStream =
    Pin<Box<dyn tokio_stream::Stream<Item = Result<ReverseForwardEvent, Status>> + Send>>;
type Event = reverse_forward_event::Event;

pub(crate) async fn serve(
    request: Request<Streaming<ReverseForwardEvent>>,
    slots: Arc<Semaphore>,
) -> Result<Response<EventStream>, Status> {
    let mut inbound = request.into_inner();
    let first = tokio::time::timeout(super::HANDSHAKE_TIMEOUT, inbound.message())
        .await
        .map_err(|_| Status::deadline_exceeded("reverse forward handshake timed out"))??
        .ok_or_else(|| Status::invalid_argument("reverse forward requires a listen event"))?;
    let Some(Event::Listen(open)) = first.event else {
        return Err(Status::invalid_argument(
            "reverse forward requires a listen event",
        ));
    };
    if open.protocol_version != PROTOCOL_VERSION {
        return Err(Status::failed_precondition(
            "unsupported agent protocol version",
        ));
    }
    if open.host.is_empty()
        || open.host.contains('\0')
        || open.port == 0
        || open.port > u16::MAX as u32
    {
        return Err(Status::invalid_argument("invalid reverse listen address"));
    }
    let listener = TcpListener::bind((open.host.as_str(), open.port as u16))
        .await
        .map_err(|error| Status::unavailable(format!("bind guest listener: {error}")))?;
    let address = listener
        .local_addr()
        .map_err(|error| Status::internal(error.to_string()))?;
    let (outbound, receiver) = mpsc::channel(32);
    outbound
        .send(Ok(ReverseForwardEvent {
            connection_id: 0,
            event: Some(Event::Ready(address.to_string())),
        }))
        .await
        .map_err(|_| Status::cancelled("reverse forward closed"))?;

    tokio::spawn(async move {
        let mut next_id = 0u64;
        let mut connections: HashMap<u64, mpsc::Sender<Event>> = HashMap::new();
        loop {
            tokio::select! {
                _ = outbound.closed() => break,
                accepted = listener.accept() => {
                    let Ok((socket, _)) = accepted else { break; };
                    let Ok(permit) = slots.clone().try_acquire_owned() else { continue; };
                    next_id += 1;
                    let id = next_id;
                    let (sender, input) = mpsc::channel(32);
                    connections.insert(id, sender);
                    if outbound.send(Ok(ReverseForwardEvent { connection_id: id, event: Some(Event::Connected(true)) })).await.is_err() { break; }
                    tokio::spawn(bridge(socket, id, input, outbound.clone(), permit));
                }
                message = inbound.message() => {
                    let Ok(Some(message)) = message else { break; };
                    let id = message.connection_id;
                    match message.event {
                        Some(event @ (Event::Data(_) | Event::Close(_))) => {
                            if let Some(sender) = connections.get(&id)
                                && sender.send(event).await.is_err()
                            {
                                connections.remove(&id);
                            }
                        }
                        _ => break,
                    }
                }
            }
            connections.retain(|_, sender| !sender.is_closed());
        }
    });
    Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
}

async fn bridge(
    socket: TcpStream,
    id: u64,
    mut input: mpsc::Receiver<Event>,
    outbound: mpsc::Sender<Result<ReverseForwardEvent, Status>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
) {
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
    outbound: &mpsc::Sender<Result<ReverseForwardEvent, Status>>,
    id: u64,
    event: Event,
) -> Result<(), mpsc::error::SendError<Result<ReverseForwardEvent, Status>>> {
    outbound
        .send(Ok(ReverseForwardEvent {
            connection_id: id,
            event: Some(event),
        }))
        .await
}
