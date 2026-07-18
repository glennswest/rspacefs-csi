//! StorageClass parameter keys and the volume-context that carries them from
//! `CreateVolume` (controller) to `NodePublishVolume` (node).
//!
//! These map 1:1 onto `rspacefs-mount --pvc` flags — see the README table.

use std::collections::HashMap;

/// OCI artifact ref(s) pulled as read-only lower layers. Comma/whitespace
/// separated for multiple lowers. Zero = empty PVC. → `--lower-blob` (0..N).
pub const SEED: &str = "seed";
/// `empty` | `ro` | `rwo` | `rwx`. → `--access-mode`.
pub const ACCESS_MODE: &str = "accessMode";
/// `persistent` | `ephemeral` | `ephemeral-then-persistent`. → `--lifecycle`.
pub const LIFECYCLE: &str = "lifecycle";
/// `UID:GID` of the workload's runAsUser. → `--owner`.
pub const OWNER: &str = "owner";
/// `"true"` to `capture-layer` the upper to a registry revision at DeleteVolume.
pub const CAPTURE_ON_DELETE: &str = "captureOnDelete";

/// Extra volume-context key set by the controller so the node knows the logical
/// PVC name (independent of the opaque volume id).
pub const VOLUME_NAME: &str = "rspacefs.csi.g8.io/volumeName";

/// Parsed, validated view of the driver-specific parameters.
#[derive(Debug, Clone, Default)]
pub struct PvcParams {
    pub seeds: Vec<String>,
    pub access_mode: Option<String>,
    pub lifecycle: Option<String>,
    pub owner: Option<String>,
    pub capture_on_delete: bool,
}

impl PvcParams {
    /// Parse from a StorageClass parameter / volume-context map. Unknown keys are
    /// ignored so CO-injected keys (csi.storage.k8s.io/*) don't cause failures.
    pub fn parse(m: &HashMap<String, String>) -> Self {
        let seeds = m
            .get(SEED)
            .map(|s| {
                s.split([',', ' ', '\n', '\t'])
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        PvcParams {
            seeds,
            access_mode: m.get(ACCESS_MODE).cloned(),
            lifecycle: m.get(LIFECYCLE).cloned(),
            owner: m.get(OWNER).cloned(),
            capture_on_delete: m
                .get(CAPTURE_ON_DELETE)
                .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
                .unwrap_or(false),
        }
    }

    /// Re-serialize into the volume-context handed back to the CO and later
    /// replayed to the node in `NodePublishVolume`.
    pub fn to_volume_context(&self) -> HashMap<String, String> {
        let mut c = HashMap::new();
        if !self.seeds.is_empty() {
            c.insert(SEED.to_string(), self.seeds.join(","));
        }
        if let Some(v) = &self.access_mode {
            c.insert(ACCESS_MODE.to_string(), v.clone());
        }
        if let Some(v) = &self.lifecycle {
            c.insert(LIFECYCLE.to_string(), v.clone());
        }
        if let Some(v) = &self.owner {
            c.insert(OWNER.to_string(), v.clone());
        }
        if self.capture_on_delete {
            c.insert(CAPTURE_ON_DELETE.to_string(), "true".to_string());
        }
        c
    }
}
