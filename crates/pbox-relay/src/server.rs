//! The relay matches scoped connections and forwards opaque binary frames.
use crate::{DEAD_PEER, HEARTBEAT, READY, authorised, valid_box_id};
use axum::{
    Router,
    extract::{
        Path, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
};
use futures_util::StreamExt;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, Notify, Semaphore, oneshot};

type Waiting = HashMap<String, (u64, oneshot::Sender<WebSocket>)>;

struct Relay {
    key: String,
    waiting: Mutex<Waiting>,
    slots: Arc<Semaphore>,
    clients: Arc<Semaphore>,
    available: Notify,
    next: std::sync::atomic::AtomicU64,
}

/// Limit waiting and active agent sessions together. No PVE credentials are used here.
pub fn router(key: String, max_connections: usize) -> anyhow::Result<Router> {
    anyhow::ensure!(
        key.len() >= 32,
        "relay key must contain at least 32 characters"
    );
    anyhow::ensure!(
        max_connections > 0,
        "max-connections must be greater than zero"
    );
    Ok(Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/v1/{role}/{box_id}", get(upgrade))
        .with_state(Arc::new(Relay {
            key,
            waiting: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(max_connections)),
            clients: Arc::new(Semaphore::new(max_connections)),
            available: Notify::new(),
            next: Default::default(),
        })))
}

async fn upgrade(
    State(state): State<Arc<Relay>>,
    Path((role, box_id)): Path<(String, String)>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !matches!(role.as_str(), "agent" | "client")
        || !valid_box_id(&box_id)
        || !authorised(&state.key, &role, &box_id, token)
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let ws = ws.max_message_size(64 * 1024).max_frame_size(64 * 1024);
    if role == "client" {
        let Ok(permit) = state.clients.clone().try_acquire_owned() else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        // A busy agent replenishes its waiting socket after handing off a session.
        // Wait for that socket so simultaneous CLI commands do not race into 503s.
        let sender = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let notified = state.available.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if let Some((_, sender)) = state.waiting.lock().await.remove(&box_id) {
                    break sender;
                }
                notified.await;
            }
        })
        .await;
        let Ok(sender) = sender else {
            return (StatusCode::SERVICE_UNAVAILABLE, "agent is not connected").into_response();
        };
        return ws.on_upgrade(move |socket| async move {
            let _permit = permit;
            let _ = sender.send(socket);
        });
    }
    let Ok(permit) = state.slots.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let (sender, receiver) = oneshot::channel();
    let generation = state
        .next
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    {
        let mut waiting = state.waiting.lock().await;
        if waiting.contains_key(&box_id) {
            return StatusCode::CONFLICT.into_response();
        }
        waiting.insert(box_id.clone(), (generation, sender));
    }
    state.available.notify_waiters();
    let failed_state = state.clone();
    let failed_box = box_id.clone();
    ws.on_failed_upgrade(move |_| {
        tokio::spawn(async move {
            remove_waiter(&failed_state, &failed_box, generation).await;
        });
    })
    .on_upgrade(move |socket| async move {
        let _permit = permit;
        let _ = wait_and_forward(socket, receiver).await;
        remove_waiter(&state, &box_id, generation).await;
    })
}

async fn remove_waiter(state: &Relay, box_id: &str, generation: u64) {
    let mut waiting = state.waiting.lock().await;
    if waiting.get(box_id).is_some_and(|(id, _)| *id == generation) {
        waiting.remove(box_id);
    }
}

async fn wait_and_forward(
    mut agent: WebSocket,
    mut receiver: oneshot::Receiver<WebSocket>,
) -> anyhow::Result<()> {
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    let deadline = tokio::time::sleep(DEAD_PEER);
    tokio::pin!(deadline);
    let mut client = loop {
        tokio::select! {
            socket = &mut receiver => break socket?,
            _ = heartbeat.tick() => agent.send(Message::Ping(Vec::new().into())).await?,
            message = agent.next() => match message.transpose()? {
                Some(Message::Pong(_)) => deadline.as_mut().reset(tokio::time::Instant::now() + DEAD_PEER),
                _ => return Ok(()),
            },
            _ = &mut deadline => return Ok(()),
        }
    };
    agent.send(Message::Text(READY.into())).await?;
    loop {
        tokio::select! {
            message = agent.next() => if !forward(message, &mut client).await? { return Ok(()); },
            message = client.next() => if !forward(message, &mut agent).await? { return Ok(()); },
        }
    }
}

async fn forward(
    message: Option<Result<Message, axum::Error>>,
    target: &mut WebSocket,
) -> anyhow::Result<bool> {
    let Some(message) = message.transpose()? else {
        return Ok(false);
    };
    let keep_open = !matches!(message, Message::Close(_));
    if matches!(message, Message::Text(_)) {
        anyhow::bail!("text frames are not session data");
    }
    tokio::time::timeout(DEAD_PEER, target.send(message)).await??;
    Ok(keep_open)
}

/// Serve on an existing listener, also used by end-to-end transport tests.
pub async fn serve(
    listener: tokio::net::TcpListener,
    key: String,
    max_connections: usize,
) -> anyhow::Result<()> {
    axum::serve(listener, router(key, max_connections)?).await?;
    Ok(())
}
