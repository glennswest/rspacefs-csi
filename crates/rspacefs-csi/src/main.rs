//! rspacefs-csi — CSI driver for rspacefs layered-filesystem data PVCs.
//!
//! One binary, run as either the controller plugin (Deployment) or the node
//! plugin (DaemonSet); the identity service runs in both. See the README for the
//! CSI-RPC → rspacefs mapping.

mod controller;
mod driver;
mod identity;
mod mount;
mod node;
mod oci;
mod params;
mod server;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use driver::{Config, Role};

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
        mount_bin: args.mount_bin,
    });

    server::serve(&args.endpoint, args.role, cfg).await
}
