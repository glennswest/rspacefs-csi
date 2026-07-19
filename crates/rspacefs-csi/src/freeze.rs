//! "Freeze" — capture a live PVC's writable upper and push it as a new data
//! revision. Shared by the node plugin (captureOnDelete at unpublish) and the
//! controller (CreateSnapshot, when the control socket is locally reachable).

use std::path::Path;

use tracing::info;

use crate::control::Capturer;
use crate::registry::Registry;

#[derive(Debug, thiserror::Error)]
pub enum FreezeError {
    #[error("capture: {0}")]
    Capture(#[from] crate::control::ControlError),
    #[error("push: {0}")]
    Push(#[from] crate::registry::RegistryError),
}

/// Capture `control_socket`'s upper into `staging`, then push it to `target_ref`
/// (`registry/repo:tag`). Returns the canonical pushed reference
/// (`registry/repo@sha256:…`) — a value directly usable as a seed for an
/// "attach a copy" clone.
pub async fn freeze_and_push(
    capturer: &dyn Capturer,
    registry: &dyn Registry,
    control_socket: &Path,
    staging: &Path,
    target_ref: &str,
) -> Result<String, FreezeError> {
    if let Some(parent) = staging.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let cap = capturer.capture(control_socket, staging).await?;
    info!(
        digest = %cap.digest,
        bytes = cap.bytes_compressed,
        entries = cap.entries,
        "captured upper"
    );
    let pushed = registry
        .push_revision(target_ref, staging, &cap.digest, cap.bytes_compressed)
        .await?;
    // Best-effort cleanup of the staging blob.
    let _ = tokio::fs::remove_file(staging).await;
    info!(reference = %pushed, "pushed data revision");
    Ok(pushed)
}
