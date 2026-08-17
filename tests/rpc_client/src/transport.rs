//! The three ways to reach a daemon, behind one trait.
//!
//! A transport turns bytes into JSON-RPC frames and back. Everything above
//! that — methods, parameters, streaming events, the auth handshake's
//! *content* — is identical on all three, which is the property the daemon
//! side claims and this is how a test gets to check it: run the same
//! exchange over each and see that only the framing differed.
//!
//! Unix and TCP are newline-delimited; WebSocket is message-framed and adds
//! no delimiter of its own.

use anyhow::Context;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

#[async_trait::async_trait]
pub trait Transport: Send {
    /// Send one frame. The JSON is already serialized; framing is the
    /// transport's business.
    async fn send(&mut self, json: String) -> anyhow::Result<()>;

    /// The next frame, or `None` when the peer closed.
    async fn recv(&mut self) -> anyhow::Result<Option<Value>>;

    /// For assertions and failure messages.
    fn name(&self) -> &'static str;
}

/// Newline-delimited JSON over anything that reads and writes bytes.
pub struct LineTransport<R, W> {
    reader: BufReader<R>,
    writer: W,
    name: &'static str,
}

#[async_trait::async_trait]
impl<R, W> Transport for LineTransport<R, W>
where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    async fn send(&mut self, json: String) -> anyhow::Result<()> {
        self.writer.write_all(json.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;
        Ok(())
    }

    async fn recv(&mut self) -> anyhow::Result<Option<Value>> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line).await?;
            if n == 0 {
                return Ok(None);
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            return Ok(Some(
                serde_json::from_str(trimmed).with_context(|| format!("bad frame: {trimmed}"))?,
            ));
        }
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

pub async fn unix(
    socket_path: &Path,
) -> anyhow::Result<LineTransport<tokio::net::unix::OwnedReadHalf, tokio::net::unix::OwnedWriteHalf>>
{
    let (reader, writer) = tokio::net::UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connecting to {}", socket_path.display()))?
        .into_split();
    Ok(LineTransport {
        reader: BufReader::new(reader),
        writer,
        name: "unix",
    })
}

pub async fn tcp(
    addr: std::net::SocketAddr,
) -> anyhow::Result<LineTransport<tokio::net::tcp::OwnedReadHalf, tokio::net::tcp::OwnedWriteHalf>>
{
    let (reader, writer) = tokio::net::TcpStream::connect(addr)
        .await
        .with_context(|| format!("connecting to {addr}"))?
        .into_split();
    Ok(LineTransport {
        reader: BufReader::new(reader),
        writer,
        name: "tcp",
    })
}

/// One frame per WebSocket text message.
pub struct WsTransport {
    inner: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

pub async fn ws(addr: std::net::SocketAddr, origin: Option<&str>) -> anyhow::Result<WsTransport> {
    let mut request = format!("ws://{addr}/").into_client_request()?;
    if let Some(origin) = origin {
        request.headers_mut().insert("origin", origin.parse()?);
    }
    let (inner, _) = tokio_tungstenite::connect_async(request)
        .await
        .with_context(|| format!("opening a WebSocket to {addr}"))?;
    Ok(WsTransport { inner })
}

#[async_trait::async_trait]
impl Transport for WsTransport {
    async fn send(&mut self, json: String) -> anyhow::Result<()> {
        self.inner.send(Message::Text(json.into())).await?;
        Ok(())
    }

    async fn recv(&mut self) -> anyhow::Result<Option<Value>> {
        loop {
            match self.inner.next().await {
                None | Some(Ok(Message::Close(_))) => return Ok(None),
                Some(Ok(Message::Text(t))) => {
                    return Ok(Some(
                        serde_json::from_str(&t).with_context(|| format!("bad frame: {t}"))?,
                    ))
                }
                // Ping/pong are tungstenite's business, not the protocol's.
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(e.into()),
            }
        }
    }

    fn name(&self) -> &'static str {
        "ws"
    }
}
