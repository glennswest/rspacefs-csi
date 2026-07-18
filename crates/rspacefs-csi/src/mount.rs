//! The `rspacefs-mount --pvc` exec wrapper used by the node plugin.
//!
//! `publish` composes the layer view (writable upper + registry-seeded lowers)
//! as a FUSE mount at the kubelet target path; `unpublish` tears it down.

use std::io;
use std::path::Path;

use tokio::fs;
use tokio::process::Command;
use tracing::{debug, info};

use crate::driver::Config;
use crate::oci;
use crate::params::PvcParams;

/// FUSE-mount the composed layer view at `target_path`.
///
/// Builds:
/// `rspacefs-mount --pvc [--lower-blob <blob> ...] --upper <dir>
///   [--owner UID:GID] [--access-mode <m>] [--lifecycle <l>] [--read-only]
///   --control-socket <sock> <target_path>`
pub async fn publish(
    cfg: &Config,
    volume_id: &str,
    target_path: &str,
    params: &PvcParams,
    readonly: bool,
) -> io::Result<()> {
    let upper = cfg.upper_dir(volume_id);
    let control = cfg.control_socket(volume_id);

    // Per-volume working dirs on the node.
    fs::create_dir_all(&upper).await?;
    // The target path is created by the kubelet as a directory for fs volumes;
    // ensure it exists so the mount has somewhere to land.
    fs::create_dir_all(target_path).await?;

    let mut cmd = Command::new(&cfg.mount_bin);
    cmd.arg("--pvc");

    // Resolve each seed to a local read-only lower blob, then pass it through.
    for seed in &params.seeds {
        let blob = oci::resolve_lower(seed).await.map_err(io::Error::other)?;
        cmd.arg("--lower-blob").arg(blob);
    }

    cmd.arg("--upper").arg(&upper);

    if let Some(owner) = &params.owner {
        cmd.arg("--owner").arg(owner);
    }
    if let Some(mode) = &params.access_mode {
        cmd.arg("--access-mode").arg(mode);
    }
    if let Some(lc) = &params.lifecycle {
        cmd.arg("--lifecycle").arg(lc);
    }
    if readonly {
        cmd.arg("--read-only");
    }

    cmd.arg("--control-socket").arg(&control);
    cmd.arg(target_path);

    debug!(?cmd, "spawning rspacefs-mount");

    // rspacefs-mount daemonizes the FUSE server and returns once the mount is
    // live, so we wait for its exit status and surface a non-zero code.
    let output = cmd.output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!(
            "rspacefs-mount exited with {}: {}",
            output.status,
            stderr.trim()
        )));
    }
    info!(volume = %volume_id, ?upper, "rspacefs-mount established");
    Ok(())
}

/// Unmount the FUSE view and reap the daemon.
pub async fn unpublish(cfg: &Config, volume_id: &str, target_path: &str) -> io::Result<()> {
    // If the target is already gone / not mounted, treat unpublish as idempotent.
    if !Path::new(target_path).exists() {
        return Ok(());
    }

    // Prefer fusermount3 (unprivileged FUSE unmount); fall back to umount.
    let unmounted = run_ok(Command::new("fusermount3").arg("-u").arg(target_path)).await
        || run_ok(Command::new("fusermount").arg("-u").arg(target_path)).await
        || run_ok(Command::new("umount").arg(target_path)).await;

    if !unmounted {
        return Err(io::Error::other(format!("could not unmount {target_path}")));
    }

    // TODO(rspacefs): signal the daemon via the control socket for a clean
    // shutdown / optional capture before removing the working dir.
    let _ = cfg.control_socket(volume_id);

    // Remove the kubelet mount point directory (best-effort).
    let _ = fs::remove_dir(target_path).await;
    Ok(())
}

/// Run a command, returning true iff it exits 0.
async fn run_ok(cmd: &mut Command) -> bool {
    match cmd.output().await {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}
