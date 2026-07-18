//! Controller service (Deployment) — the OCI-artifact side of the driver.
//!
//! `CreateVolume` resolves the seed artifact(s) and mints a logical volume;
//! `DeleteVolume` optionally captures the upper back to the registry;
//! `CreateSnapshot` captures the upper as a pushable/pullable registry revision.
//! The node-local upper directory and FUSE mount are handled by the node plugin.

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

use crate::oci;
use crate::params::{PvcParams, VOLUME_NAME};

#[derive(Debug, Default)]
pub struct ControllerService;

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

        let params = PvcParams::parse(&req.parameters);

        // If provisioning from a snapshot, that revision becomes an extra lower.
        let mut params = params;
        if let Some(src) = &req.volume_content_source {
            if let Some(csi_proto::volume_content_source::Type::Snapshot(s)) = &src.r#type {
                info!(snapshot = %s.snapshot_id, "seeding volume from snapshot");
                params.seeds.push(s.snapshot_id.clone());
            }
        }

        // Resolve (pull metadata / verify existence of) each seed artifact.
        // TODO(rspacefs): actually pull blobs into the shared content store.
        for seed in &params.seeds {
            oci::resolve_lower(seed)
                .await
                .map_err(|e| Status::failed_precondition(format!("seed {seed}: {e}")))?;
        }

        // The volume id is the CO-provided name; rspacefs volumes are named,
        // not block-extent handles.
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
        // captureOnDelete is carried in the (secret-free) volume context that the
        // CO replays on delete; here we only have volume_id + secrets, so the
        // policy is enforced by the node/registry side.
        // TODO(rspacefs): capture-layer the upper if captureOnDelete, then release.
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

        // TODO(rspacefs): capture-layer → deterministic tar+zstd OCI artifact,
        // deduped by digest. The snapshot id IS the registry revision ref.
        let snapshot_id = oci::snapshot_ref(&req.source_volume_id, &req.name);
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
        // TODO(rspacefs): drop the registry revision if unreferenced.
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
        // rspacefs is single-consumer; we confirm whatever the CO asked for and
        // let the node enforce access-mode at mount time.
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

    // ---- Unsupported controller RPCs (advertised capabilities exclude these) ----

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

fn now_ts() -> prost_types::Timestamp {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    prost_types::Timestamp {
        seconds: d.as_secs() as i64,
        nanos: d.subsec_nanos() as i32,
    }
}
