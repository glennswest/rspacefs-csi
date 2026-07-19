//! gRPC server: binds the CSI unix domain socket and serves the enabled
//! services until SIGINT/SIGTERM.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use csi_proto::controller_server::ControllerServer;
use csi_proto::identity_server::IdentityServer;
use csi_proto::node_server::NodeServer;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::info;

use crate::control::Capturer;
use crate::controller::ControllerService;
use crate::driver::{Config, Role};
use crate::identity::IdentityService;
use crate::mount::Mounter;
use crate::node::NodeService;
use crate::registry::Registry;

/// The backends the services are wired to. Real impls in `main`; mocks in tests.
pub struct Backends {
    pub mounter: Arc<dyn Mounter>,
    pub registry: Arc<dyn Registry>,
    pub capturer: Arc<dyn Capturer>,
}

/// Parse a CSI endpoint (`unix:///csi/csi.sock`, `unix://relative.sock`, or a
/// bare path) into a filesystem path.
pub fn endpoint_path(endpoint: &str) -> PathBuf {
    let stripped = endpoint.strip_prefix("unix://").unwrap_or(endpoint);
    PathBuf::from(stripped)
}

pub async fn serve(
    endpoint: &str,
    role: Role,
    cfg: Arc<Config>,
    backends: Backends,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = endpoint_path(endpoint);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // A stale socket from a previous run would make bind() fail with EADDRINUSE.
    remove_if_socket(&path)?;

    let listener = UnixListener::bind(&path)?;
    let incoming = UnixListenerStream::new(listener);

    let controller = role.serves_controller().then(|| {
        ControllerServer::new(ControllerService::new(
            cfg.clone(),
            backends.registry.clone(),
            backends.capturer.clone(),
        ))
    });
    let node = role.serves_node().then(|| {
        NodeServer::new(NodeService::new(
            cfg.clone(),
            backends.mounter.clone(),
            backends.registry.clone(),
            backends.capturer.clone(),
        ))
    });

    let router = Server::builder()
        .add_service(IdentityServer::new(IdentityService))
        .add_optional_service(controller)
        .add_optional_service(node);

    info!(
        driver = crate::driver::DRIVER_NAME,
        version = crate::driver::DRIVER_VERSION,
        ?role,
        socket = %path.display(),
        "serving CSI gRPC"
    );

    router
        .serve_with_incoming_shutdown(incoming, shutdown_signal())
        .await?;

    let _ = std::fs::remove_file(&path);
    Ok(())
}

fn remove_if_socket(path: &Path) -> std::io::Result<()> {
    match std::fs::metadata(path) {
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Resolve when the process receives SIGINT or SIGTERM.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => info!("received SIGTERM, shutting down"),
            _ = int.recv() => info!("received SIGINT, shutting down"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
