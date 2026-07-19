//! Client for a running `rspacefs-mount` daemon's control socket.
//!
//! The daemon (started with `--control-socket <path>`) listens on a unix socket
//! speaking newline-delimited JSON: one request object per line, one response
//! line back (`crates/rspacefs-fuse/src/control.rs`). We use it for the "freeze"
//! op — `capture-layer` snapshots the writable upper into a deterministic
//! tar+zstd blob and returns its digest, which we then push as a data revision.

use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("io connecting to control socket {0}: {1}")]
    Connect(String, #[source] std::io::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("daemon closed the connection without responding")]
    NoResponse,
    #[error("malformed response: {0}")]
    Protocol(String),
    #[error("daemon error: {0}")]
    Remote(String),
}

/// `data` payload of a successful `capture-layer` response.
#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)] // out_path echoes our request; kept for logging/debugging.
pub struct CaptureData {
    pub out_path: String,
    /// `sha256:…` of the captured tar+zstd blob.
    pub digest: String,
    pub bytes_compressed: u64,
    pub entries: usize,
}

/// Response envelope: `{ "ok": true, "data": {...} }` or `{ "ok": false, "error": "..." }`.
#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    #[serde(default = "Option::default")]
    data: Option<T>,
    #[serde(default = "Option::default")]
    error: Option<String>,
}

/// The control operations the driver invokes on a live PVC mount.
#[async_trait]
pub trait Capturer: Send + Sync {
    /// `capture-layer` — freeze the current upper into `out_path` (tar+zstd) and
    /// return its digest/size/entry count.
    async fn capture(&self, socket: &Path, out_path: &Path) -> Result<CaptureData, ControlError>;
}

/// Real client speaking the unix-socket JSON protocol.
#[derive(Default)]
pub struct SocketControl;

impl SocketControl {
    /// Send one request object and decode the single-line response envelope.
    async fn request<T: for<'de> Deserialize<'de>>(
        &self,
        socket: &Path,
        request: &serde_json::Value,
    ) -> Result<T, ControlError> {
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|e| ControlError::Connect(socket.display().to_string(), e))?;
        let (read_half, mut write_half) = stream.into_split();

        let mut line =
            serde_json::to_vec(request).map_err(|e| ControlError::Protocol(e.to_string()))?;
        line.push(b'\n');
        write_half.write_all(&line).await?;
        write_half.flush().await?;

        let mut reader = BufReader::new(read_half);
        let mut resp = String::new();
        if reader.read_line(&mut resp).await? == 0 {
            return Err(ControlError::NoResponse);
        }

        let env: Envelope<T> = serde_json::from_str(resp.trim())
            .map_err(|e| ControlError::Protocol(format!("{e}: {}", resp.trim())))?;
        if !env.ok {
            return Err(ControlError::Remote(
                env.error.unwrap_or_else(|| "unspecified error".into()),
            ));
        }
        env.data
            .ok_or_else(|| ControlError::Protocol("ok=true but no data".into()))
    }
}

#[async_trait]
impl Capturer for SocketControl {
    async fn capture(&self, socket: &Path, out_path: &Path) -> Result<CaptureData, ControlError> {
        let req = serde_json::json!({
            "cmd": "capture-layer",
            "out_path": out_path,
        });
        self.request(socket, &req).await
    }
}
