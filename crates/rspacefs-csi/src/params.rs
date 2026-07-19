//! StorageClass parameter keys and the volume-context that carries them from
//! `CreateVolume` (controller) to `NodePublishVolume` (node).
//!
//! These map 1:1 onto `rspacefs-mount --pvc` flags — see `docs/pvc.md` in the
//! rspacefs repo and the README table.

use std::collections::HashMap;

/// OCI data-artifact ref(s) pulled as read-only lower layers. Comma/whitespace
/// separated for multiple lowers. Zero = empty PVC. → `--lower-blob` (0..N).
pub const SEED: &str = "seed";
/// `empty` | `ro` | `rwo` | `rwx`. → `--access-mode`.
pub const ACCESS_MODE: &str = "accessMode";
/// `persistent` | `ephemeral` | `ephemeral-then-persistent`. → `--lifecycle`.
pub const LIFECYCLE: &str = "lifecycle";
/// `UID:GID` of the workload's runAsUser. → `--owner`.
pub const OWNER: &str = "owner";
/// `"true"` to `capture-layer` the upper to a registry revision at unpublish/delete.
pub const CAPTURE_ON_DELETE: &str = "captureOnDelete";
/// OCI ref the captured upper is pushed to (`registry/repo`), for
/// captureOnDelete and snapshots. Defaults to the first seed's registry+repo.
pub const CAPTURE_REPO: &str = "captureRepo";

/// Extra volume-context key set by the controller so the node knows the logical
/// PVC name (independent of the opaque volume id).
pub const VOLUME_NAME: &str = "rspacefs.csi.g8.io/volumeName";

/// PVC access mode — maps to `--access-mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    Empty,
    Ro,
    Rwo,
    Rwx,
}

impl AccessMode {
    pub fn as_flag(self) -> &'static str {
        match self {
            AccessMode::Empty => "empty",
            AccessMode::Ro => "ro",
            AccessMode::Rwo => "rwo",
            AccessMode::Rwx => "rwx",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "empty" => Some(AccessMode::Empty),
            "ro" | "readonly" => Some(AccessMode::Ro),
            "rwo" | "readwriteonce" => Some(AccessMode::Rwo),
            "rwx" | "readwritemany" => Some(AccessMode::Rwx),
            _ => None,
        }
    }
}

/// PVC lifecycle — maps to `--lifecycle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    Persistent,
    Ephemeral,
    EphemeralThenPersistent,
}

impl Lifecycle {
    pub fn as_flag(self) -> &'static str {
        match self {
            Lifecycle::Persistent => "persistent",
            Lifecycle::Ephemeral => "ephemeral",
            Lifecycle::EphemeralThenPersistent => "ephemeral-then-persistent",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "persistent" => Some(Lifecycle::Persistent),
            "ephemeral" => Some(Lifecycle::Ephemeral),
            "ephemeral-then-persistent" | "ephemeralthenpersistent" => {
                Some(Lifecycle::EphemeralThenPersistent)
            }
            _ => None,
        }
    }
}

/// Parsed, validated view of the driver-specific parameters.
#[derive(Debug, Clone, Default)]
pub struct PvcParams {
    pub seeds: Vec<String>,
    pub access_mode: Option<AccessMode>,
    pub lifecycle: Option<Lifecycle>,
    pub owner: Option<String>,
    pub capture_on_delete: bool,
    pub capture_repo: Option<String>,
}

/// A rejected parameter value.
#[derive(Debug, thiserror::Error)]
#[error("invalid {key} {value:?}: {reason}")]
pub struct ParamError {
    pub key: &'static str,
    pub value: String,
    pub reason: &'static str,
}

impl PvcParams {
    /// Parse from a StorageClass parameter / volume-context map. Unknown keys are
    /// ignored so CO-injected keys (`csi.storage.k8s.io/*`) don't cause failures.
    pub fn parse(m: &HashMap<String, String>) -> Result<Self, ParamError> {
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

        let access_mode = match m.get(ACCESS_MODE) {
            Some(v) => Some(AccessMode::parse(v).ok_or(ParamError {
                key: ACCESS_MODE,
                value: v.clone(),
                reason: "expected empty|ro|rwo|rwx",
            })?),
            None => None,
        };
        let lifecycle = match m.get(LIFECYCLE) {
            Some(v) => Some(Lifecycle::parse(v).ok_or(ParamError {
                key: LIFECYCLE,
                value: v.clone(),
                reason: "expected persistent|ephemeral|ephemeral-then-persistent",
            })?),
            None => None,
        };

        Ok(PvcParams {
            seeds,
            access_mode,
            lifecycle,
            owner: m.get(OWNER).cloned(),
            capture_on_delete: m
                .get(CAPTURE_ON_DELETE)
                .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
                .unwrap_or(false),
            capture_repo: m.get(CAPTURE_REPO).cloned(),
        })
    }

    /// Re-serialize into the volume-context handed back to the CO and later
    /// replayed to the node in `NodePublishVolume`.
    pub fn to_volume_context(&self) -> HashMap<String, String> {
        let mut c = HashMap::new();
        if !self.seeds.is_empty() {
            c.insert(SEED.to_string(), self.seeds.join(","));
        }
        if let Some(v) = self.access_mode {
            c.insert(ACCESS_MODE.to_string(), v.as_flag().to_string());
        }
        if let Some(v) = self.lifecycle {
            c.insert(LIFECYCLE.to_string(), v.as_flag().to_string());
        }
        if let Some(v) = &self.owner {
            c.insert(OWNER.to_string(), v.clone());
        }
        if self.capture_on_delete {
            c.insert(CAPTURE_ON_DELETE.to_string(), "true".to_string());
        }
        if let Some(v) = &self.capture_repo {
            c.insert(CAPTURE_REPO.to_string(), v.clone());
        }
        c
    }

    /// The registry repo captures/snapshots of this volume push to. Explicit
    /// `captureRepo` wins; otherwise derive `registry/repository` from the first
    /// seed (dropping its tag/digest).
    pub fn resolved_capture_repo(&self) -> Option<String> {
        if let Some(repo) = &self.capture_repo {
            return Some(repo.clone());
        }
        let seed = self.seeds.first()?;
        // Strip a trailing `:tag` or `@digest` to get `registry/repository`.
        let base = seed.split('@').next().unwrap_or(seed);
        let repo = match base.rfind('/') {
            Some(slash) => match base[slash + 1..].rfind(':') {
                Some(colon) => &base[..slash + 1 + colon],
                None => base,
            },
            None => base,
        };
        Some(repo.to_string())
    }
}
