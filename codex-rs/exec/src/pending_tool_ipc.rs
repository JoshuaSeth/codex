use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use codex_core::CodexThread;
use codex_core::protocol::Op;
use codex_protocol::models::FunctionCallOutputPayload;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::select;
use tokio::sync::oneshot;

#[derive(Deserialize, Serialize)]
struct DeliverPendingRequest {
    call_id: String,
    output: FunctionCallOutputPayload,
}

pub struct PendingToolServer {
    shutdown_tx: Option<oneshot::Sender<()>>,
    addr: SocketAddr,
}

#[derive(Debug, Deserialize)]
pub struct PendingToolSocketMetadata {
    pub host: String,
    pub port: u16,
}

impl PendingToolServer {
    pub async fn start(conversation: Arc<CodexThread>) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("failed to bind pending tool listener")?;
        let addr = listener
            .local_addr()
            .context("listener missing local addr")?;
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

        tokio::spawn(async move {
            loop {
                select! {
                    _ = &mut shutdown_rx => {
                        break;
                    }
                    accept_result = listener.accept() => {
                        match accept_result {
                            Ok((stream, _)) => {
                                let convo = Arc::clone(&conversation);
                                tokio::spawn(async move {
                                    if let Err(err) = handle_connection(stream, convo).await {
                                        tracing::warn!("pending tool IPC error: {err:?}");
                                    }
                                });
                            }
                            Err(err) => {
                                tracing::warn!("pending tool listener accept error: {err:?}");
                            }
                        }
                    }
                }
            }
        });

        Ok(Self {
            shutdown_tx: Some(shutdown_tx),
            addr,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for PendingToolServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    conversation: Arc<CodexThread>,
) -> anyhow::Result<()> {
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    if buf.is_empty() {
        return Ok(());
    }
    let request: DeliverPendingRequest = serde_json::from_slice(&buf)?;
    conversation
        .submit(Op::DeliverPendingToolResult {
            call_id: request.call_id,
            output: request.output,
        })
        .await?;
    stream.write_all(b"ok").await?;
    Ok(())
}

pub async fn send_pending_result(
    addr: SocketAddr,
    call_id: String,
    output: FunctionCallOutputPayload,
) -> anyhow::Result<()> {
    let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .context("pending tool IPC connect timeout")?
        .context("failed to connect to pending tool listener")?;
    let request = DeliverPendingRequest { call_id, output };
    let body = serde_json::to_vec(&request)?;
    stream.write_all(&body).await?;
    stream.shutdown().await?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut buf))
        .await
        .context("pending tool IPC response timeout")??;
    Ok(())
}

pub fn addr_from_metadata(meta: PendingToolSocketMetadata) -> anyhow::Result<SocketAddr> {
    let host: IpAddr = meta.host.parse().context("invalid pending tool host")?;
    Ok(SocketAddr::new(host, meta.port))
}

pub fn load_metadata(contents: Value) -> anyhow::Result<PendingToolSocketMetadata> {
    let meta: PendingToolSocketMetadata = serde_json::from_value(contents)?;
    Ok(meta)
}
