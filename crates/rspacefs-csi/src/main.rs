//! rspacefs-csi — CSI driver for rspacefs layered-filesystem **data** PVCs.
//!
//! One binary, run as either the controller plugin (Deployment) or the node
//! plugin (DaemonSet); the identity service runs in both. It is data-volume
//! only — container rootfs images are handled by CRI-O + rspacefs, not here.
//! See the README for the CSI-RPC → rspacefs mapping.

// tonic's `Status` is a large error type; every CSI RPC returns it, so allowing
// this lint crate-wide keeps our own helpers consistent with the trait surface.
#![allow(clippy::result_large_err)]

mod control;
mod controller;
mod driver;
mod freeze;
mod identity;
mod mount;
mod node;
mod params;
mod registry;
mod server;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use control::SocketControl;
use driver::{Config, Role};
use mount::RspacefsMounter;
use registry::OciClient;
use server::Backends;

/// CSI driver for rspacefs layered-filesystem data PVCs.
#[derive(Debug, Parser)]
#[command(name = "rspacefs-csi", version, about)]
struct Args {
    /// CSI unix socket endpoint the CO connects to.
    #[arg(long, env = "CSI_ENDPOINT", default_value = "unix:///csi/csi.sock")]
    endpoint: String,

    /// Which service(s) to run.
    #[arg(long, env = "CSI_ROLE", value_enum, default_value_t = Role::All)]
    role: Role,

    /// This node's name (required for the node role).
    #[arg(long, env = "NODE_ID", default_value = "")]
    node_id: String,

    /// Root for per-volume upper dirs, pulled lower blobs, and control sockets.
    #[arg(
        long,
        env = "RSPACEFS_DATA_DIR",
        default_value = "/var/lib/rspacefs-csi"
    )]
    data_dir: PathBuf,

    /// Path to the rspacefs-mount binary the node plugin execs.
    #[arg(long, env = "RSPACEFS_MOUNT_BIN", default_value = "rspacefs-mount")]
    mount_bin: PathBuf,

    /// Registry scheme for OCI pulls/pushes (`https` or `http`).
    #[arg(long, env = "RSPACEFS_REGISTRY_SCHEME", default_value = "https")]
    registry_scheme: String,

    /// Accept invalid TLS certs (self-signed local registries like qregistry.local).
    #[arg(long, env = "RSPACEFS_REGISTRY_INSECURE", default_value_t = false)]
    registry_insecure: bool,

    /// Default `registry[/prefix]` for captured revisions when none is derivable
    /// from a seed or `captureRepo` (e.g. `qregistry.local`).
    #[arg(long, env = "RSPACEFS_CAPTURE_REGISTRY")]
    capture_registry: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,rspacefs_csi=debug")),
        )
        .init();

    let args = Args::parse();

    if args.role.serves_node() && args.node_id.is_empty() {
        return Err("node role requires --node-id / NODE_ID".into());
    }

    let cfg = Arc::new(Config {
        node_id: args.node_id,
        data_dir: args.data_dir,
        capture_registry: args.capture_registry,
    });

    let backends = Backends {
        mounter: Arc::new(RspacefsMounter {
            mount_bin: args.mount_bin,
        }),
        registry: Arc::new(OciClient::new(
            args.registry_scheme,
            args.registry_insecure,
        )?),
        capturer: Arc::new(SocketControl),
    };

    server::serve(&args.endpoint, args.role, cfg, backends).await
}
