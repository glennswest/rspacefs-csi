//! Identity service — served by every role.

use csi_proto::identity_server::Identity;
use csi_proto::{
    plugin_capability::{service::Type as ServiceType, Service, Type as CapType},
    GetPluginCapabilitiesRequest, GetPluginCapabilitiesResponse, GetPluginInfoRequest,
    GetPluginInfoResponse, PluginCapability, ProbeRequest, ProbeResponse,
};
use tonic::{Request, Response, Status};

use crate::driver::{DRIVER_NAME, DRIVER_VERSION};

#[derive(Debug, Default)]
pub struct IdentityService;

#[tonic::async_trait]
impl Identity for IdentityService {
    async fn get_plugin_info(
        &self,
        _req: Request<GetPluginInfoRequest>,
    ) -> Result<Response<GetPluginInfoResponse>, Status> {
        Ok(Response::new(GetPluginInfoResponse {
            name: DRIVER_NAME.to_string(),
            vendor_version: DRIVER_VERSION.to_string(),
            manifest: Default::default(),
        }))
    }

    async fn get_plugin_capabilities(
        &self,
        _req: Request<GetPluginCapabilitiesRequest>,
    ) -> Result<Response<GetPluginCapabilitiesResponse>, Status> {
        // We run a controller service (CreateVolume/*Snapshot). rspacefs is not
        // topology-constrained, so no VOLUME_ACCESSIBILITY_CONSTRAINTS.
        let caps = vec![PluginCapability {
            r#type: Some(CapType::Service(Service {
                r#type: ServiceType::ControllerService as i32,
            })),
        }];
        Ok(Response::new(GetPluginCapabilitiesResponse {
            capabilities: caps,
        }))
    }

    async fn probe(&self, _req: Request<ProbeRequest>) -> Result<Response<ProbeResponse>, Status> {
        Ok(Response::new(ProbeResponse { ready: Some(true) }))
    }
}
