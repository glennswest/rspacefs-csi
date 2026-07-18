//! Node service (DaemonSet) — the mount side of the driver.
//!
//! Every CSI op here is mount-shaped. There is no block device: Stage/Unstage
//! are no-ops, and `NodePublishVolume` execs `rspacefs-mount --pvc` to FUSE-mount
//! the composed layer view at the kubelet target path.

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

use crate::driver::Config;
use crate::mount;
use crate::params::PvcParams;

pub struct NodeService {
    cfg: Arc<Config>,
}

impl NodeService {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self { cfg }
    }
}

fn cap(rpc: CapRpc) -> NodeServiceCapability {
    NodeServiceCapability {
        r#type: Some(CapType::Rpc(Rpc { r#type: rpc as i32 })),
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
        // No block device to format/attach — nothing to stage.
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

        let params = PvcParams::parse(&req.volume_context);

        mount::publish(
            &self.cfg,
            &req.volume_id,
            &req.target_path,
            &params,
            req.readonly,
        )
        .await
        .map_err(|e| {
            warn!(volume = %req.volume_id, error = %e, "publish failed");
            Status::internal(format!("rspacefs-mount failed: {e}"))
        })?;

        info!(volume = %req.volume_id, target = %req.target_path, "published volume");
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

        mount::unpublish(&self.cfg, &req.volume_id, &req.target_path)
            .await
            .map_err(|e| {
                warn!(volume = %req.volume_id, error = %e, "unpublish failed");
                Status::internal(format!("unmount failed: {e}"))
            })?;

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
