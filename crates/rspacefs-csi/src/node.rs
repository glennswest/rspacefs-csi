//! Node service (DaemonSet) — the mount side of the driver.
//!
//! Every op here is mount-shaped. Stage/Unstage are no-ops; `NodePublishVolume`
//! pulls the seed data-artifact lower(s) and execs `rspacefs-mount --pvc` to
//! FUSE-mount the composed layer view; `NodeUnpublishVolume` optionally freezes
//! the upper into a new data revision (captureOnDelete / hand-off) then unmounts.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use csi_proto::node_server::Node;
use csi_proto::{
    node_service_capability::{rpc::Type as CapRpc, Rpc, Type as CapType},
    NodeExpandVolumeRequest, NodeExpandVolumeResponse, NodeGetCapabilitiesRequest,
    NodeGetCapabilitiesResponse, NodeGetInfoRequest, NodeGetInfoResponse,
    NodeGetVolumeStatsRequest, NodeGetVolumeStatsResponse, NodePublishVolumeRequest,
    NodePublishVolumeResponse, NodeServiceCapability, NodeStageVolumeRequest,
    NodeStageVolumeResponse, NodeUnpublishVolumeRequest, NodeUnpublishVolumeResponse,
    NodeUnstageVolumeRequest, NodeUnstageVolumeResponse,
};
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::control::Capturer;
use crate::driver::Config;
use crate::mount::{MountSpec, Mounter};
use crate::params::{AccessMode, Lifecycle, PvcParams, VOLUME_NAME};
use crate::registry::Registry;

pub struct NodeService {
    cfg: Arc<Config>,
    mounter: Arc<dyn Mounter>,
    registry: Arc<dyn Registry>,
    capturer: Arc<dyn Capturer>,
}

impl NodeService {
    pub fn new(
        cfg: Arc<Config>,
        mounter: Arc<dyn Mounter>,
        registry: Arc<dyn Registry>,
        capturer: Arc<dyn Capturer>,
    ) -> Self {
        Self {
            cfg,
            mounter,
            registry,
            capturer,
        }
    }
}

fn cap(rpc: CapRpc) -> NodeServiceCapability {
    NodeServiceCapability {
        r#type: Some(CapType::Rpc(Rpc { r#type: rpc as i32 })),
    }
}

/// Effective access mode: an explicit CSI read-only publish forces `ro`;
/// otherwise the param, else `empty` for a seedless PVC / `rwo` when seeded.
fn effective_access_mode(params: &PvcParams, readonly: bool) -> AccessMode {
    if readonly {
        return AccessMode::Ro;
    }
    params.access_mode.unwrap_or({
        if params.seeds.is_empty() {
            AccessMode::Empty
        } else {
            AccessMode::Rwo
        }
    })
}

impl NodeService {
    /// Persisted publish context path (NodeUnpublish gets no volume_context, so
    /// we stash it at publish to honor captureOnDelete on the way out).
    fn state_path(&self, volume_id: &str) -> std::path::PathBuf {
        self.cfg.volume_dir(volume_id).join("publish.json")
    }
}

#[tonic::async_trait]
impl Node for NodeService {
    async fn node_get_info(
        &self,
        _req: Request<NodeGetInfoRequest>,
    ) -> Result<Response<NodeGetInfoResponse>, Status> {
        Ok(Response::new(NodeGetInfoResponse {
            node_id: self.cfg.node_id.clone(),
            max_volumes_per_node: 0,
            accessible_topology: None,
        }))
    }

    async fn node_get_capabilities(
        &self,
        _req: Request<NodeGetCapabilitiesRequest>,
    ) -> Result<Response<NodeGetCapabilitiesResponse>, Status> {
        // Stage/Unstage are advertised but no-op; the real work is in Publish.
        Ok(Response::new(NodeGetCapabilitiesResponse {
            capabilities: vec![cap(CapRpc::StageUnstageVolume)],
        }))
    }

    async fn node_stage_volume(
        &self,
        _req: Request<NodeStageVolumeRequest>,
    ) -> Result<Response<NodeStageVolumeResponse>, Status> {
        Ok(Response::new(NodeStageVolumeResponse {}))
    }

    async fn node_unstage_volume(
        &self,
        _req: Request<NodeUnstageVolumeRequest>,
    ) -> Result<Response<NodeUnstageVolumeResponse>, Status> {
        Ok(Response::new(NodeUnstageVolumeResponse {}))
    }

    async fn node_publish_volume(
        &self,
        req: Request<NodePublishVolumeRequest>,
    ) -> Result<Response<NodePublishVolumeResponse>, Status> {
        let req = req.into_inner();
        if req.volume_id.is_empty() {
            return Err(Status::invalid_argument("volume_id is required"));
        }
        if req.target_path.is_empty() {
            return Err(Status::invalid_argument("target_path is required"));
        }

        let params = PvcParams::parse(&req.volume_context)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let access_mode = effective_access_mode(&params, req.readonly);
        let lifecycle = params.lifecycle.unwrap_or(Lifecycle::Persistent);

        let upper = self.cfg.upper_dir(&req.volume_id);
        let lowers_dir = self.cfg.lowers_dir(&req.volume_id);
        let control_socket = self.cfg.control_socket(&req.volume_id);

        // Pull each seed's data-artifact layers into the per-volume lowers dir,
        // ordered by seed then top-down within a seed.
        let mut lower_blobs = Vec::new();
        for seed in &params.seeds {
            let blobs = self
                .registry
                .pull_layers(seed, &lowers_dir)
                .await
                .map_err(|e| Status::failed_precondition(format!("seed {seed}: {e}")))?;
            lower_blobs.extend(blobs);
        }

        tokio::fs::create_dir_all(&upper)
            .await
            .map_err(|e| Status::internal(format!("create upper: {e}")))?;
        tokio::fs::create_dir_all(&req.target_path)
            .await
            .map_err(|e| Status::internal(format!("create target: {e}")))?;

        let name = req
            .volume_context
            .get(VOLUME_NAME)
            .cloned()
            .unwrap_or_else(|| req.volume_id.clone());

        let spec = MountSpec {
            name,
            target: req.target_path.clone().into(),
            upper,
            control_socket,
            access_mode,
            lifecycle,
            owner: params.owner.clone(),
            lower_blobs,
        };

        self.mounter.mount(&spec).await.map_err(|e| {
            warn!(volume = %req.volume_id, error = %e, "mount failed");
            Status::internal(format!("rspacefs-mount: {e}"))
        })?;

        // Stash the publish context for NodeUnpublish (which gets none).
        if let Err(e) = self
            .persist_state(&req.volume_id, &req.volume_context)
            .await
        {
            warn!(volume = %req.volume_id, error = %e, "could not persist publish state");
        }

        info!(volume = %req.volume_id, target = %req.target_path, mode = access_mode.as_flag(), "published volume");
        Ok(Response::new(NodePublishVolumeResponse {}))
    }

    async fn node_unpublish_volume(
        &self,
        req: Request<NodeUnpublishVolumeRequest>,
    ) -> Result<Response<NodeUnpublishVolumeResponse>, Status> {
        let req = req.into_inner();
        if req.volume_id.is_empty() {
            return Err(Status::invalid_argument("volume_id is required"));
        }
        if req.target_path.is_empty() {
            return Err(Status::invalid_argument("target_path is required"));
        }

        // Recover the publish context; capture-on-handoff needs it and the live
        // daemon, so it must run before we unmount.
        if let Some(ctx) = self.load_state(&req.volume_id).await {
            if let Ok(params) = PvcParams::parse(&ctx) {
                if params.capture_on_delete {
                    self.freeze_on_unpublish(&req.volume_id, &ctx, &params)
                        .await?;
                }
            }
        }

        self.mounter
            .unmount(Path::new(&req.target_path))
            .await
            .map_err(|e| {
                warn!(volume = %req.volume_id, error = %e, "unmount failed");
                Status::internal(format!("unmount: {e}"))
            })?;

        // Best-effort cleanup of the kubelet mount point and publish state.
        let _ = tokio::fs::remove_dir(&req.target_path).await;
        let _ = tokio::fs::remove_file(self.state_path(&req.volume_id)).await;

        info!(volume = %req.volume_id, target = %req.target_path, "unpublished volume");
        Ok(Response::new(NodeUnpublishVolumeResponse {}))
    }

    async fn node_get_volume_stats(
        &self,
        _req: Request<NodeGetVolumeStatsRequest>,
    ) -> Result<Response<NodeGetVolumeStatsResponse>, Status> {
        Err(Status::unimplemented("GET_VOLUME_STATS is not advertised"))
    }

    async fn node_expand_volume(
        &self,
        _req: Request<NodeExpandVolumeRequest>,
    ) -> Result<Response<NodeExpandVolumeResponse>, Status> {
        Err(Status::unimplemented("EXPAND_VOLUME is not advertised"))
    }
}

impl NodeService {
    async fn persist_state(
        &self,
        volume_id: &str,
        ctx: &HashMap<String, String>,
    ) -> std::io::Result<()> {
        let path = self.state_path(volume_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let bytes = serde_json::to_vec(ctx)?;
        tokio::fs::write(path, bytes).await
    }

    async fn load_state(&self, volume_id: &str) -> Option<HashMap<String, String>> {
        let bytes = tokio::fs::read(self.state_path(volume_id)).await.ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Freeze the live upper into a new data revision before unmount.
    async fn freeze_on_unpublish(
        &self,
        volume_id: &str,
        ctx: &HashMap<String, String>,
        params: &PvcParams,
    ) -> Result<(), Status> {
        let control_socket = self.cfg.control_socket(volume_id);
        let repo = params.resolved_capture_repo().ok_or_else(|| {
            Status::failed_precondition(
                "captureOnDelete set but no captureRepo and no seed to derive one from",
            )
        })?;
        let name = ctx
            .get(VOLUME_NAME)
            .cloned()
            .unwrap_or_else(|| volume_id.to_string());
        let target_ref = format!("{repo}:{}", sanitize_tag(&name));
        let staging = self.cfg.capture_staging(volume_id);

        let pushed = crate::freeze::freeze_and_push(
            self.capturer.as_ref(),
            self.registry.as_ref(),
            &control_socket,
            &staging,
            &target_ref,
        )
        .await
        .map_err(|e| Status::internal(format!("capture-on-delete: {e}")))?;
        info!(volume = %volume_id, reference = %pushed, "captured volume on unpublish");
        Ok(())
    }
}

/// Reduce an arbitrary PVC name to a valid OCI tag (`[A-Za-z0-9_.-]{1,128}`).
pub fn sanitize_tag(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    out.truncate(128);
    if out.is_empty() {
        out.push_str("rev");
    }
    out
}
