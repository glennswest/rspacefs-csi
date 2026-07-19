//! Controller service (Deployment) — the data-artifact side of the driver.
//!
//! `CreateVolume` mints a logical PVC and validates its seed data-artifact(s);
//! `CreateSnapshot` freezes a live volume's upper into a new registry revision;
//! `DeleteVolume`/`DeleteSnapshot` release. The node-local upper directory and
//! FUSE mount are the node plugin's job.
//!
//! Snapshotting needs the volume's live control socket, which lives on the node
//! hosting the volume. When the controller shares that host (role `all`, or a
//! hostPath data-dir), it freezes directly; otherwise it returns
//! FailedPrecondition telling the caller to snapshot from the node.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use csi_proto::controller_server::Controller;
use csi_proto::{
    controller_service_capability::{rpc::Type as CapRpc, Rpc, Type as CapType},
    ControllerExpandVolumeRequest, ControllerExpandVolumeResponse,
    ControllerGetCapabilitiesRequest, ControllerGetCapabilitiesResponse,
    ControllerGetVolumeRequest, ControllerGetVolumeResponse, ControllerModifyVolumeRequest,
    ControllerModifyVolumeResponse, ControllerPublishVolumeRequest,
    ControllerPublishVolumeResponse, ControllerServiceCapability, ControllerUnpublishVolumeRequest,
    ControllerUnpublishVolumeResponse, CreateSnapshotRequest, CreateSnapshotResponse,
    CreateVolumeRequest, CreateVolumeResponse, DeleteSnapshotRequest, DeleteSnapshotResponse,
    DeleteVolumeRequest, DeleteVolumeResponse, GetCapacityRequest, GetCapacityResponse,
    ListSnapshotsRequest, ListSnapshotsResponse, ListVolumesRequest, ListVolumesResponse, Snapshot,
    ValidateVolumeCapabilitiesRequest, ValidateVolumeCapabilitiesResponse, Volume,
};
use tonic::{Request, Response, Status};
use tracing::info;

use crate::control::Capturer;
use crate::driver::Config;
use crate::node::sanitize_tag;
use crate::params::{PvcParams, CAPTURE_REPO, VOLUME_NAME};
use crate::registry::Registry;

pub struct ControllerService {
    cfg: Arc<Config>,
    registry: Arc<dyn Registry>,
    capturer: Arc<dyn Capturer>,
}

impl ControllerService {
    pub fn new(cfg: Arc<Config>, registry: Arc<dyn Registry>, capturer: Arc<dyn Capturer>) -> Self {
        Self {
            cfg,
            registry,
            capturer,
        }
    }
}

fn cap(rpc: CapRpc) -> ControllerServiceCapability {
    ControllerServiceCapability {
        r#type: Some(CapType::Rpc(Rpc { r#type: rpc as i32 })),
    }
}

#[tonic::async_trait]
impl Controller for ControllerService {
    async fn controller_get_capabilities(
        &self,
        _req: Request<ControllerGetCapabilitiesRequest>,
    ) -> Result<Response<ControllerGetCapabilitiesResponse>, Status> {
        Ok(Response::new(ControllerGetCapabilitiesResponse {
            // CREATE_DELETE_SNAPSHOT covers "freeze"; "attach a copy to another
            // container" is then provision-from-snapshot, which needs no extra
            // capability (the external-provisioner passes it as a content source).
            capabilities: vec![
                cap(CapRpc::CreateDeleteVolume),
                cap(CapRpc::CreateDeleteSnapshot),
            ],
        }))
    }

    async fn create_volume(
        &self,
        req: Request<CreateVolumeRequest>,
    ) -> Result<Response<CreateVolumeResponse>, Status> {
        let req = req.into_inner();
        if req.name.is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        if req.volume_capabilities.is_empty() {
            return Err(Status::invalid_argument("volume_capabilities is required"));
        }

        let mut params = PvcParams::parse(&req.parameters)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        // Provision-from-snapshot / clone: the source revision becomes a lower,
        // so the new volume is a copy-on-write copy for another container.
        if let Some(src) = &req.volume_content_source {
            if let Some(csi_proto::volume_content_source::Type::Snapshot(s)) = &src.r#type {
                info!(snapshot = %s.snapshot_id, "seeding volume from snapshot");
                params.seeds.push(s.snapshot_id.clone());
            }
        }

        // Validate each seed data-artifact exists (fail fast on a bad ref).
        for seed in &params.seeds {
            self.registry
                .resolve(seed)
                .await
                .map_err(|e| Status::failed_precondition(format!("seed {seed}: {e}")))?;
        }

        let volume_id = req.name.clone();
        let mut ctx = params.to_volume_context();
        ctx.insert(VOLUME_NAME.to_string(), req.name.clone());

        let capacity_bytes = req
            .capacity_range
            .as_ref()
            .map(|r| r.required_bytes.max(r.limit_bytes))
            .unwrap_or(0);

        info!(volume = %volume_id, seeds = params.seeds.len(), "created volume");
        Ok(Response::new(CreateVolumeResponse {
            volume: Some(Volume {
                capacity_bytes,
                volume_id,
                volume_context: ctx,
                content_source: req.volume_content_source,
                accessible_topology: vec![],
            }),
        }))
    }

    async fn delete_volume(
        &self,
        req: Request<DeleteVolumeRequest>,
    ) -> Result<Response<DeleteVolumeResponse>, Status> {
        let req = req.into_inner();
        if req.volume_id.is_empty() {
            return Err(Status::invalid_argument("volume_id is required"));
        }
        // The upper is node-local and any captureOnDelete freeze already happened
        // at NodeUnpublish; the logical volume carries no controller-side state.
        info!(volume = %req.volume_id, "deleted volume");
        Ok(Response::new(DeleteVolumeResponse {}))
    }

    async fn create_snapshot(
        &self,
        req: Request<CreateSnapshotRequest>,
    ) -> Result<Response<CreateSnapshotResponse>, Status> {
        let req = req.into_inner();
        if req.source_volume_id.is_empty() {
            return Err(Status::invalid_argument("source_volume_id is required"));
        }
        if req.name.is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }

        let target_ref = self.snapshot_target(&req.parameters, &req.source_volume_id, &req.name)?;
        let control_socket = self.cfg.control_socket(&req.source_volume_id);
        if !control_socket.exists() {
            return Err(Status::failed_precondition(format!(
                "volume {} is not mounted on this host; take the snapshot from the node hosting it",
                req.source_volume_id
            )));
        }
        let staging = self.cfg.capture_staging(&req.source_volume_id);

        let snapshot_id = crate::freeze::freeze_and_push(
            self.capturer.as_ref(),
            self.registry.as_ref(),
            &control_socket,
            &staging,
            &target_ref,
        )
        .await
        .map_err(|e| Status::internal(format!("snapshot: {e}")))?;

        info!(snapshot = %snapshot_id, source = %req.source_volume_id, "created snapshot");
        Ok(Response::new(CreateSnapshotResponse {
            snapshot: Some(Snapshot {
                size_bytes: 0,
                snapshot_id,
                source_volume_id: req.source_volume_id,
                creation_time: Some(now_ts()),
                ready_to_use: true,
                group_snapshot_id: String::new(),
            }),
        }))
    }

    async fn delete_snapshot(
        &self,
        req: Request<DeleteSnapshotRequest>,
    ) -> Result<Response<DeleteSnapshotResponse>, Status> {
        let req = req.into_inner();
        if req.snapshot_id.is_empty() {
            return Err(Status::invalid_argument("snapshot_id is required"));
        }
        // The snapshot id is a registry revision ref; blob GC in rspace_registry
        // reclaims it once unreferenced. Nothing controller-side to release.
        info!(snapshot = %req.snapshot_id, "deleted snapshot");
        Ok(Response::new(DeleteSnapshotResponse {}))
    }

    async fn validate_volume_capabilities(
        &self,
        req: Request<ValidateVolumeCapabilitiesRequest>,
    ) -> Result<Response<ValidateVolumeCapabilitiesResponse>, Status> {
        let req = req.into_inner();
        if req.volume_id.is_empty() {
            return Err(Status::invalid_argument("volume_id is required"));
        }
        Ok(Response::new(ValidateVolumeCapabilitiesResponse {
            confirmed: Some(
                csi_proto::validate_volume_capabilities_response::Confirmed {
                    volume_context: req.volume_context,
                    volume_capabilities: req.volume_capabilities,
                    parameters: req.parameters,
                    mutable_parameters: Default::default(),
                },
            ),
            message: String::new(),
        }))
    }

    // ---- Unsupported controller RPCs (not advertised) ----

    async fn controller_publish_volume(
        &self,
        _req: Request<ControllerPublishVolumeRequest>,
    ) -> Result<Response<ControllerPublishVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "rspacefs has no attach step; PUBLISH_UNPUBLISH_VOLUME is not advertised",
        ))
    }

    async fn controller_unpublish_volume(
        &self,
        _req: Request<ControllerUnpublishVolumeRequest>,
    ) -> Result<Response<ControllerUnpublishVolumeResponse>, Status> {
        Err(Status::unimplemented("not supported"))
    }

    async fn list_volumes(
        &self,
        _req: Request<ListVolumesRequest>,
    ) -> Result<Response<ListVolumesResponse>, Status> {
        Err(Status::unimplemented("LIST_VOLUMES is not advertised"))
    }

    async fn get_capacity(
        &self,
        _req: Request<GetCapacityRequest>,
    ) -> Result<Response<GetCapacityResponse>, Status> {
        Err(Status::unimplemented("GET_CAPACITY is not advertised"))
    }

    async fn list_snapshots(
        &self,
        _req: Request<ListSnapshotsRequest>,
    ) -> Result<Response<ListSnapshotsResponse>, Status> {
        Err(Status::unimplemented("LIST_SNAPSHOTS is not advertised"))
    }

    async fn controller_expand_volume(
        &self,
        _req: Request<ControllerExpandVolumeRequest>,
    ) -> Result<Response<ControllerExpandVolumeResponse>, Status> {
        Err(Status::unimplemented("EXPAND_VOLUME is not advertised"))
    }

    async fn controller_get_volume(
        &self,
        _req: Request<ControllerGetVolumeRequest>,
    ) -> Result<Response<ControllerGetVolumeResponse>, Status> {
        Err(Status::unimplemented("GET_VOLUME is not advertised"))
    }

    async fn controller_modify_volume(
        &self,
        _req: Request<ControllerModifyVolumeRequest>,
    ) -> Result<Response<ControllerModifyVolumeResponse>, Status> {
        Err(Status::unimplemented("MODIFY_VOLUME is not advertised"))
    }
}

impl ControllerService {
    /// Resolve where a snapshot of `source_volume_id` named `name` is pushed:
    /// the `captureRepo` snapshot-class param, else `<capture_registry>/pvcs/<id>`.
    fn snapshot_target(
        &self,
        params: &HashMap<String, String>,
        source_volume_id: &str,
        name: &str,
    ) -> Result<String, Status> {
        let repo = if let Some(repo) = params.get(CAPTURE_REPO) {
            repo.clone()
        } else if let Some(reg) = &self.cfg.capture_registry {
            format!("{reg}/pvcs/{source_volume_id}")
        } else {
            return Err(Status::failed_precondition(
                "no captureRepo parameter and no --capture-registry configured",
            ));
        };
        Ok(format!("{repo}:{}", sanitize_tag(name)))
    }
}

fn now_ts() -> prost_types::Timestamp {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    prost_types::Timestamp {
        seconds: d.as_secs() as i64,
        nanos: d.subsec_nanos() as i32,
    }
}
