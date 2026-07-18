//! Driver-wide identity and configuration.

use std::path::PathBuf;

/// CSI driver name advertised via `GetPluginInfo`. Must match the `name` in
/// `deploy/csidriver.yaml` and the `provisioner` of any StorageClass.
pub const DRIVER_NAME: &str = "rspacefs.csi.g8.io";

/// Reported to the CO as the plugin's `vendor_version`.
pub const DRIVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Which half of the driver this process runs as. Identity is always served;
/// the controller and node services are gated by the deployment role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Role {
    /// Controller plugin (Deployment): CreateVolume/DeleteVolume/*Snapshot.
    Controller,
    /// Node plugin (DaemonSet): NodePublishVolume → `rspacefs-mount --pvc`.
    Node,
    /// Both services in one process (useful for local testing).
    All,
}

impl Role {
    pub fn serves_controller(self) -> bool {
        matches!(self, Role::Controller | Role::All)
    }
    pub fn serves_node(self) -> bool {
        matches!(self, Role::Node | Role::All)
    }
}

/// Resolved runtime configuration shared by the service implementations.
#[derive(Debug, Clone)]
pub struct Config {
    /// This node's name (from `NODE_ID`/`KUBE_NODE_NAME`); only meaningful on the node.
    pub node_id: String,
    /// Root under which per-volume upper dirs, pulled lower blobs, and control
    /// sockets live on the node (e.g. `/var/lib/rspacefs-csi`).
    pub data_dir: PathBuf,
    /// Path to the `rspacefs-mount` binary the node plugin execs.
    pub mount_bin: PathBuf,
}

impl Config {
    /// `<data_dir>/volumes/<volume_id>` — the per-volume working directory.
    pub fn volume_dir(&self, volume_id: &str) -> PathBuf {
        self.data_dir.join("volumes").join(volume_id)
    }
    /// The writable upper directory passed to `rspacefs-mount --upper`.
    pub fn upper_dir(&self, volume_id: &str) -> PathBuf {
        self.volume_dir(volume_id).join("upper")
    }
    /// The FUSE daemon control socket passed to `rspacefs-mount --control-socket`.
    pub fn control_socket(&self, volume_id: &str) -> PathBuf {
        self.volume_dir(volume_id).join("control.sock")
    }
}
