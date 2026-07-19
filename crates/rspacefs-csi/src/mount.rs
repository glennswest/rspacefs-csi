//! The `rspacefs-mount --pvc` exec wrapper used by the node plugin, behind a
//! [`Mounter`] trait so node logic unit-tests without a real FUSE mount.
//!
//! Command shape (per rspacefs `docs/pvc.md`):
//! ```text
//! rspacefs-mount --pvc --name <id> --access-mode <m> --lifecycle <l>
//!   [--lower-blob <pulled-path>]... --upper <dir> [--owner UID:GID]
//!   --control-socket <sock> <target_path>
//! ```
//! There is no `--read-only` flag — read-only is `--access-mode ro`.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::process::Command;
use tracing::debug;

use crate::params::{AccessMode, Lifecycle};

/// Everything needed to bring up one PVC FUSE mount.
#[derive(Debug, Clone)]
pub struct MountSpec {
    /// PVC identifier (`--name`), used in logs and capture filenames.
    pub name: String,
    /// Kubelet target path the merged view is mounted at.
    pub target: PathBuf,
    /// Writable upper directory (`--upper`).
    pub upper: PathBuf,
    /// Daemon control socket (`--control-socket`).
    pub control_socket: PathBuf,
    pub access_mode: AccessMode,
    pub lifecycle: Lifecycle,
    /// `UID:GID` for `--owner`, if set.
    pub owner: Option<String>,
    /// Pulled read-only lower blob paths, top-down (`--lower-blob`, repeatable).
    pub lower_blobs: Vec<PathBuf>,
}

impl MountSpec {
    /// Render the argv for `rspacefs-mount` (excluding argv[0]).
    pub fn args(&self) -> Vec<String> {
        let mut a = vec!["--pvc".into(), "--name".into(), self.name.clone()];
        a.push("--access-mode".into());
        a.push(self.access_mode.as_flag().into());
        a.push("--lifecycle".into());
        a.push(self.lifecycle.as_flag().into());
        for blob in &self.lower_blobs {
            a.push("--lower-blob".into());
            a.push(blob.display().to_string());
        }
        a.push("--upper".into());
        a.push(self.upper.display().to_string());
        if let Some(owner) = &self.owner {
            a.push("--owner".into());
            a.push(owner.clone());
        }
        a.push("--control-socket".into());
        a.push(self.control_socket.display().to_string());
        a.push(self.target.display().to_string());
        a
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("rspacefs-mount exited {status}: {stderr}")]
    MountFailed { status: String, stderr: String },
    #[error("could not unmount {0}")]
    UnmountFailed(String),
}

/// FUSE mount operations. The real impl execs `rspacefs-mount`; tests use a mock.
#[async_trait]
pub trait Mounter: Send + Sync {
    async fn mount(&self, spec: &MountSpec) -> Result<(), MountError>;
    async fn unmount(&self, target: &Path) -> Result<(), MountError>;
}

/// Real mounter: shells out to `rspacefs-mount` and `fusermount3`/`umount`.
pub struct RspacefsMounter {
    pub mount_bin: PathBuf,
}

#[async_trait]
impl Mounter for RspacefsMounter {
    async fn mount(&self, spec: &MountSpec) -> Result<(), MountError> {
        let args = spec.args();
        debug!(bin = %self.mount_bin.display(), ?args, "spawning rspacefs-mount");
        // rspacefs-mount daemonizes once the kernel acknowledges the mount, so
        // waiting for exit returns promptly with a status.
        let output = Command::new(&self.mount_bin).args(&args).output().await?;
        if !output.status.success() {
            return Err(MountError::MountFailed {
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(())
    }

    async fn unmount(&self, target: &Path) -> Result<(), MountError> {
        if !target.exists() {
            // Idempotent: nothing mounted / already cleaned up.
            return Ok(());
        }
        let t = target.display().to_string();
        let ok = run_ok("fusermount3", &["-u", &t]).await
            || run_ok("fusermount", &["-u", &t]).await
            || run_ok("umount", &[&t]).await;
        if !ok {
            return Err(MountError::UnmountFailed(t));
        }
        Ok(())
    }
}

async fn run_ok(bin: &str, args: &[&str]) -> bool {
    matches!(
        Command::new(bin).args(args).output().await,
        Ok(o) if o.status.success()
    )
}
