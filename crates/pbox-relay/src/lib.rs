//! Outbound WebSocket transport. The byte stream carries the agent's existing mutual TLS.

pub mod server;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const HEARTBEAT: Duration = Duration::from_secs(15);
const DEAD_PEER: Duration = Duration::from_secs(60);
const READY: &str = "pbox-relay-v1-ready";
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A guest receives only its own agent token, never the relay's master key.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayAccess {
    pub url: String,
    pub token: String,
}

impl std::fmt::Debug for RelayAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayAccess")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Derive a role-bound capability. Knowledge of one guest token does not grant other routes.
pub fn scoped_token(master: &str, role: &str, box_id: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(master.as_bytes()).expect("HMAC accepts any key length");
    mac.update(format!("pbox-relay-v1/{role}/{box_id}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub(crate) fn authorised(master: &str, role: &str, box_id: &str, token: &str) -> bool {
    let Ok(bytes) = hex::decode(token) else {
        return false;
    };
    let mut mac =
        Hmac::<Sha256>::new_from_slice(master.as_bytes()).expect("HMAC accepts any key length");
    mac.update(format!("pbox-relay-v1/{role}/{box_id}").as_bytes());
    mac.verify_slice(&bytes).is_ok()
}

/// Validate an HTTP(S) or WS(S) relay origin, including private IPv4 and IPv6 addresses.
pub fn websocket_url(origin: &str, role: &str, box_id: &str) -> Result<String> {
    if !matches!(role, "agent" | "client") || !valid_box_id(box_id) {
        bail!("invalid relay role or box ID");
    }
    let mut url = url::Url::parse(origin).context("parse relay URL")?;
    let scheme = match url.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        _ => bail!("relay URL must use http, https, ws, or wss"),
    };
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        bail!("relay URL must be an origin without credentials, path, query, or fragment");
    }
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid relay URL scheme"))?;
    url.set_path(&format!("/v1/{role}/{box_id}"));
    Ok(url.to_string())
}

pub(crate) fn valid_box_id(value: &str) -> bool {
    value.starts_with("pbx_")
        && value.len() == 12
        && value[4..]
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

async fn open(access: &RelayAccess, role: &str, box_id: &str) -> Result<Socket> {
    let url = websocket_url(&access.url, role, box_id)?;
    let mut request = url.into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {}", access.token).parse()?);
    // https://docs.rs/tokio-tungstenite/0.28.0/tokio_tungstenite/fn.connect_async.html
    let (socket, _) =
        tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request))
            .await
            .context("relay connection timed out")?
            .context("connect to relay")?;
    Ok(socket)
}

/// Open a client stream. TLS authentication takes place above this opaque transport.
pub async fn connect(access: &RelayAccess, box_id: &str) -> Result<DuplexStream> {
    let socket = open(access, "client", box_id).await?;
    let (stream, tunnel) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let _ = bridge(socket, tunnel).await;
    });
    Ok(stream)
}

/// Keep one waiting connection, and permit up to 32 independent active client connections.
/// Reconnects restore availability; a broken live shell is reported to its caller, not replayed.
pub async fn run_agent(access: RelayAccess, box_id: String, local: SocketAddr) -> Result<()> {
    let slots = Arc::new(Semaphore::new(32));
    loop {
        let permit = slots.clone().acquire_owned().await?;
        let result = async {
            let mut socket = open(&access, "agent", &box_id).await?;
            loop {
                let message = tokio::time::timeout(DEAD_PEER, socket.next())
                    .await
                    .context("relay heartbeat timed out")?
                    .context("relay closed waiting connection")??;
                match message {
                    Message::Text(text) if text == READY => break,
                    Message::Ping(bytes) => socket.send(Message::Pong(bytes)).await?,
                    Message::Close(_) => bail!("relay closed waiting connection"),
                    _ => bail!("invalid relay waiting message"),
                }
            }
            Ok::<_, anyhow::Error>(socket)
        }
        .await;
        match result {
            Ok(socket) => {
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Ok(stream) = TcpStream::connect(local).await {
                        let _ = bridge(socket, stream).await;
                    }
                });
            }
            Err(error) => {
                eprintln!("[relay] {error:#}; reconnecting in 2 seconds");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

async fn bridge<S: AsyncRead + AsyncWrite + Unpin>(mut socket: Socket, stream: S) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut buffer = vec![0; 32 * 1024];
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    let deadline = tokio::time::sleep(DEAD_PEER);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            message = socket.next() => {
                deadline.as_mut().reset(tokio::time::Instant::now() + DEAD_PEER);
                match message.transpose()? {
                    Some(Message::Binary(bytes)) => tokio::time::timeout(DEAD_PEER, writer.write_all(&bytes)).await??,
                    Some(Message::Ping(bytes)) => socket.send(Message::Pong(bytes)).await?,
                    Some(Message::Pong(_)) => {},
                    Some(Message::Close(_)) | None => { writer.shutdown().await?; return Ok(()); },
                    _ => bail!("relay sent non-binary session data"),
                }
            }
            result = reader.read(&mut buffer) => {
                let length = result?;
                if length == 0 { let _ = socket.close(None).await; return Ok(()); }
                tokio::time::timeout(DEAD_PEER, socket.send(Message::Binary(buffer[..length].to_vec().into()))).await??;
            }
            _ = heartbeat.tick() => { tokio::time::timeout(DEAD_PEER, socket.send(Message::Ping(Vec::new().into()))).await??; }
            _ = &mut deadline => bail!("relay peer stopped responding"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const KEY: &str = "test-only-relay-master-key-32-characters";
    const BOX: &str = "pbx_12345678";

    #[test]
    fn credentials_are_scoped_to_role_and_box() {
        let token = scoped_token(KEY, "agent", BOX);
        assert!(authorised(KEY, "agent", BOX, &token));
        assert!(!authorised(KEY, "client", BOX, &token));
        assert!(!authorised(KEY, "agent", "pbx_abcdefgh", &token));
        assert!(!authorised(KEY, "agent", BOX, "bad-token"));
    }

    #[test]
    fn endpoints_accept_hostnames_and_private_addresses_without_url_secrets() {
        assert_eq!(
            websocket_url("https://pbox.example.com", "agent", BOX).unwrap(),
            "wss://pbox.example.com/v1/agent/pbx_12345678"
        );
        assert!(websocket_url("http://100.64.0.2:8080", "client", BOX).is_ok());
        assert!(websocket_url("http://[::1]:8080", "client", BOX).is_ok());
        for origin in [
            "https://user:secret@example.com",
            "https://example.com/?token=x",
            "https://example.com/path",
            "file:///tmp/socket",
        ] {
            assert!(websocket_url(origin, "agent", BOX).is_err());
        }
    }

    #[tokio::test]
    async fn relay_transports_binary_data_and_reconnects_for_next_client() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, server::router(KEY.to_owned(), 32).unwrap())
                .await
                .unwrap();
        });
        let local = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = local.local_addr().unwrap();
        let echo = tokio::spawn(async move {
            loop {
                let (mut stream, _) = local.accept().await.unwrap();
                tokio::spawn(async move {
                    let (mut read, mut write) = stream.split();
                    let _ = tokio::io::copy(&mut read, &mut write).await;
                });
            }
        });
        let guest = RelayAccess {
            url: url.clone(),
            token: scoped_token(KEY, "agent", BOX),
        };
        assert!(open(&guest, "client", BOX).await.is_err());
        let agent = tokio::spawn(run_agent(guest, BOX.to_owned(), address));
        let access = RelayAccess {
            url,
            token: scoped_token(KEY, "client", BOX),
        };
        futures_util::future::join_all((0..8).map(|_| async {
            let mut stream = tokio::time::timeout(Duration::from_secs(5), connect(&access, BOX))
                .await
                .unwrap()
                .unwrap();
            let payload: Vec<u8> = (0..200_000).map(|i| (i % 256) as u8).collect();
            let (mut read, mut write) = tokio::io::split(&mut stream);
            let mut received = vec![0; payload.len()];
            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::try_join!(write.write_all(&payload), read.read_exact(&mut received))
                    .unwrap();
            })
            .await
            .unwrap();
            assert_eq!(payload, received);
        }))
        .await;
        // A later command must still connect after the concurrent batch closes.
        connect(&access, BOX).await.unwrap();
        agent.abort();
        echo.abort();
        server.abort();
    }
}
