//! OCI registry client for rspacefs **data** artifacts.
//!
//! rspacefs-csi is data-volume only — it never handles container rootfs images.
//! A PVC's seed is a data artifact pulled from `rspace_registry` as read-only
//! lower layer(s); a captured upper is pushed back as a new data revision.
//!
//! rspacefs itself deliberately does not push (`enhancements/pvc-registry-content.md`:
//! "rspacefs hands a tarball + digest to the boot agent; the boot agent does the
//! OCI push") — as the CSI driver, that push is our job. We speak plain OCI
//! Distribution v2 (the same surface `rspace_registry` serves and `rspaced-oci`
//! pulls over).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt;

/// Media type of the tar+zstd layer produced by `capture-layer`.
pub const LAYER_ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";
/// OCI image manifest media type.
pub const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
/// OCI "empty" config — marks the manifest as an artifact, not a runnable image.
pub const EMPTY_CONFIG: &str = "application/vnd.oci.empty.v1+json";
/// `artifactType` stamped on captured revisions so nothing downstream mistakes a
/// PVC data artifact for a container image.
pub const PVC_ARTIFACT_TYPE: &str = "application/vnd.rspacefs.pvc.layer.v1+tar+zstd";

/// Bytes and digest of the canonical OCI empty config (`{}`).
const EMPTY_CONFIG_BYTES: &[u8] = b"{}";
const EMPTY_CONFIG_DIGEST: &str =
    "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";

/// Accept header covering the manifest/index types a seed might be served as.
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, \
application/vnd.oci.image.index.v1+json, \
application/vnd.docker.distribution.manifest.v2+json, \
application/vnd.docker.distribution.manifest.list.v2+json";

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("invalid reference {0:?}: {1}")]
    BadReference(String, String),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("registry {0} returned {1}")]
    Status(String, reqwest::StatusCode),
    #[error("digest mismatch for {reference}: expected {expected}, got {got}")]
    DigestMismatch {
        reference: String,
        expected: String,
        got: String,
    },
    #[error("seed {0} has no pullable layers")]
    NoLayers(String),
    #[error("bad response from {0}: {1}")]
    BadResponse(String, String),
}

/// A parsed OCI reference: `registry/repository[:tag|@sha256:…]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub registry: String,
    pub repository: String,
    /// Tag or `sha256:…` digest.
    pub reference: String,
}

impl Reference {
    /// Parse `registry/repository[:tag|@digest]` (tag defaults to `latest`).
    /// The registry host must be explicit (no Docker-Hub shorthand).
    pub fn parse(s: &str) -> Result<Self, RegistryError> {
        let bad = |m: &str| RegistryError::BadReference(s.to_string(), m.to_string());
        let (name, reference) = if let Some((n, d)) = s.split_once('@') {
            (n.to_string(), d.to_string())
        } else {
            let slash = s.rfind('/').map(|i| i + 1).unwrap_or(0);
            match s[slash..].rfind(':') {
                Some(colon) => (
                    s[..slash + colon].to_string(),
                    s[slash + colon + 1..].to_string(),
                ),
                None => (s.to_string(), "latest".to_string()),
            }
        };
        let (registry, repository) = name
            .split_once('/')
            .ok_or_else(|| bad("missing registry host (expected registry/repo)"))?;
        if registry.is_empty() || repository.is_empty() || reference.is_empty() {
            return Err(bad("empty registry, repository, or reference"));
        }
        Ok(Self {
            registry: registry.to_string(),
            repository: repository.to_string(),
            reference: reference.to_string(),
        })
    }
}

/// One layer of a resolved seed. `media_type`/`size` are informational (the
/// pull path is digest-addressed); kept for logging and future use.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct LayerRef {
    pub media_type: String,
    pub digest: String,
    pub size: u64,
}

/// Registry operations the driver needs. Abstracted so the controller/node
/// logic unit-tests against an in-memory mock (see [`tests`](crate::tests)).
#[async_trait]
pub trait Registry: Send + Sync {
    /// Verify a seed data artifact exists and return its layers ordered
    /// top-down (first = highest-priority lower). Used by the controller to
    /// fail `CreateVolume` fast on a bad seed.
    async fn resolve(&self, seed: &str) -> Result<Vec<LayerRef>, RegistryError>;

    /// Pull every layer of `seed` into `dir`, digest-verified. Returns the
    /// local blob paths ordered top-down for `rspacefs-mount --lower-blob`.
    async fn pull_layers(&self, seed: &str, dir: &Path) -> Result<Vec<PathBuf>, RegistryError>;

    /// Push a captured tar+zstd upper (`blob_path`, already hashed by
    /// `capture-layer`) as a new PVC data revision at `target_ref`. Returns the
    /// canonical pushed reference (`registry/repo@sha256:…`).
    async fn push_revision(
        &self,
        target_ref: &str,
        blob_path: &Path,
        layer_digest: &str,
        layer_size: u64,
    ) -> Result<String, RegistryError>;
}

// ---- minimal manifest types (only the fields we read/write) ----

#[derive(Debug, Deserialize)]
struct WireDescriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    #[serde(default)]
    size: u64,
}

#[derive(Debug, Deserialize)]
struct WireManifest {
    #[serde(default)]
    layers: Vec<WireDescriptor>,
    /// Present on an image index; we reject those (a data artifact is single).
    #[serde(default)]
    manifests: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct OutDescriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
}

#[derive(Serialize)]
struct OutManifest {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(rename = "mediaType")]
    media_type: String,
    #[serde(rename = "artifactType")]
    artifact_type: String,
    config: OutDescriptor,
    layers: Vec<OutDescriptor>,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    annotations: std::collections::BTreeMap<String, String>,
}

/// Real OCI Distribution v2 client over `reqwest`.
pub struct OciClient {
    http: reqwest::Client,
    scheme: String,
}

impl OciClient {
    /// `scheme` is `https` or `http`; `insecure` accepts invalid TLS certs
    /// (self-signed local registries like `qregistry.local`).
    pub fn new(scheme: impl Into<String>, insecure: bool) -> Result<Self, RegistryError> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("rspacefs-csi/", env!("CARGO_PKG_VERSION")))
            .danger_accept_invalid_certs(insecure)
            .build()?;
        Ok(Self {
            http,
            scheme: scheme.into(),
        })
    }

    fn manifest_url(&self, r: &Reference) -> String {
        format!(
            "{}://{}/v2/{}/manifests/{}",
            self.scheme, r.registry, r.repository, r.reference
        )
    }
    fn blob_url(&self, r: &Reference, digest: &str) -> String {
        format!(
            "{}://{}/v2/{}/blobs/{}",
            self.scheme, r.registry, r.repository, digest
        )
    }
    fn uploads_url(&self, registry: &str, repository: &str) -> String {
        format!(
            "{}://{}/v2/{}/blobs/uploads/",
            self.scheme, registry, repository
        )
    }

    async fn fetch_manifest(&self, r: &Reference) -> Result<Vec<u8>, RegistryError> {
        let url = self.manifest_url(r);
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::ACCEPT, MANIFEST_ACCEPT)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(RegistryError::Status(url, resp.status()));
        }
        Ok(resp.bytes().await?.to_vec())
    }

    async fn parse_layers(
        &self,
        r: &Reference,
        body: &[u8],
    ) -> Result<Vec<LayerRef>, RegistryError> {
        let m: WireManifest = serde_json::from_slice(body)
            .map_err(|e| RegistryError::BadResponse(r.repository.clone(), e.to_string()))?;
        if !m.manifests.is_empty() {
            return Err(RegistryError::BadResponse(
                r.repository.clone(),
                "seed is a multi-arch index; a PVC data artifact must be a single manifest".into(),
            ));
        }
        if m.layers.is_empty() {
            return Err(RegistryError::NoLayers(r.repository.clone()));
        }
        // Manifest layers are ordered base→top; rspacefs wants top-down.
        Ok(m.layers
            .into_iter()
            .rev()
            .map(|d| LayerRef {
                media_type: d.media_type,
                digest: d.digest,
                size: d.size,
            })
            .collect())
    }

    /// HEAD a blob; true if the registry already has it.
    async fn blob_exists(
        &self,
        registry: &str,
        repository: &str,
        digest: &str,
    ) -> Result<bool, RegistryError> {
        let r = Reference {
            registry: registry.to_string(),
            repository: repository.to_string(),
            reference: digest.to_string(),
        };
        let resp = self.http.head(self.blob_url(&r, digest)).send().await?;
        Ok(resp.status().is_success())
    }

    /// Upload `bytes` as a blob named by `digest` (monolithic two-step upload).
    async fn push_blob(
        &self,
        registry: &str,
        repository: &str,
        digest: &str,
        bytes: Vec<u8>,
    ) -> Result<(), RegistryError> {
        if self.blob_exists(registry, repository, digest).await? {
            return Ok(());
        }
        // Start an upload session.
        let start = self.uploads_url(registry, repository);
        let resp = self.http.post(&start).send().await?;
        if resp.status() != reqwest::StatusCode::ACCEPTED {
            return Err(RegistryError::Status(start, resp.status()));
        }
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                RegistryError::BadResponse(
                    repository.to_string(),
                    "upload POST had no Location".into(),
                )
            })?
            .to_string();
        let put_url = self.resolve_location(registry, &location, digest);
        let resp = self
            .http
            .put(&put_url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(RegistryError::Status(put_url, resp.status()));
        }
        Ok(())
    }

    /// Resolve an upload `Location` (absolute or path-only) and append the digest.
    fn resolve_location(&self, registry: &str, location: &str, digest: &str) -> String {
        let base = if location.starts_with("http://") || location.starts_with("https://") {
            location.to_string()
        } else {
            format!("{}://{}{}", self.scheme, registry, location)
        };
        let sep = if base.contains('?') { '&' } else { '?' };
        format!("{base}{sep}digest={digest}")
    }
}

#[async_trait]
impl Registry for OciClient {
    async fn resolve(&self, seed: &str) -> Result<Vec<LayerRef>, RegistryError> {
        let r = Reference::parse(seed)?;
        let body = self.fetch_manifest(&r).await?;
        self.parse_layers(&r, &body).await
    }

    async fn pull_layers(&self, seed: &str, dir: &Path) -> Result<Vec<PathBuf>, RegistryError> {
        let r = Reference::parse(seed)?;
        let layers = self.resolve(seed).await?;
        tokio::fs::create_dir_all(dir).await?;

        let mut paths = Vec::with_capacity(layers.len());
        for layer in layers {
            // Content-addressed cache: <dir>/<algo>-<hex>.blob. If it already
            // exists (verified previously), reuse it.
            let fname = format!("{}.blob", layer.digest.replace(':', "-"));
            let out = dir.join(fname);
            if tokio::fs::try_exists(&out).await.unwrap_or(false) {
                paths.push(out);
                continue;
            }
            let tmp = out.with_extension("blob.partial");
            self.pull_blob_to(&r, &layer.digest, &tmp).await?;
            tokio::fs::rename(&tmp, &out).await?;
            paths.push(out);
        }
        Ok(paths)
    }

    async fn push_revision(
        &self,
        target_ref: &str,
        blob_path: &Path,
        layer_digest: &str,
        layer_size: u64,
    ) -> Result<String, RegistryError> {
        let r = Reference::parse(target_ref)?;

        // 1. Upload the captured tar+zstd layer blob.
        let layer_bytes = tokio::fs::read(blob_path).await?;
        // Trust the caller's digest, but guard against a truncated staging file.
        if layer_bytes.len() as u64 != layer_size {
            return Err(RegistryError::BadResponse(
                r.repository.clone(),
                format!(
                    "captured blob size {} != reported {}",
                    layer_bytes.len(),
                    layer_size
                ),
            ));
        }
        self.push_blob(&r.registry, &r.repository, layer_digest, layer_bytes)
            .await?;

        // 2. Upload the empty config blob (marks this as an artifact).
        self.push_blob(
            &r.registry,
            &r.repository,
            EMPTY_CONFIG_DIGEST,
            EMPTY_CONFIG_BYTES.to_vec(),
        )
        .await?;

        // 3. Assemble and push the artifact manifest.
        let mut annotations = std::collections::BTreeMap::new();
        annotations.insert(
            "org.opencontainers.image.title".to_string(),
            r.reference.clone(),
        );
        annotations.insert("io.g8.rspacefs.pvc".to_string(), "true".to_string());
        let manifest = OutManifest {
            schema_version: 2,
            media_type: MANIFEST.to_string(),
            artifact_type: PVC_ARTIFACT_TYPE.to_string(),
            config: OutDescriptor {
                media_type: EMPTY_CONFIG.to_string(),
                digest: EMPTY_CONFIG_DIGEST.to_string(),
                size: EMPTY_CONFIG_BYTES.len() as u64,
            },
            layers: vec![OutDescriptor {
                media_type: LAYER_ZSTD.to_string(),
                digest: layer_digest.to_string(),
                size: layer_size,
            }],
            annotations,
        };
        let manifest_bytes = serde_json::to_vec(&manifest)
            .map_err(|e| RegistryError::BadResponse(r.repository.clone(), e.to_string()))?;
        let manifest_digest = format!("sha256:{}", hex::encode(Sha256::digest(&manifest_bytes)));

        let url = self.manifest_url(&r);
        let resp = self
            .http
            .put(&url)
            .header(reqwest::header::CONTENT_TYPE, MANIFEST)
            .body(manifest_bytes)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(RegistryError::Status(url, resp.status()));
        }
        // Prefer the server-reported digest; fall back to our computed one.
        let digest = resp
            .headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .map(String::from)
            .unwrap_or(manifest_digest);
        Ok(format!("{}/{}@{}", r.registry, r.repository, digest))
    }
}

impl OciClient {
    /// Stream a blob to `out`, verifying its sha256 matches `digest`.
    async fn pull_blob_to(
        &self,
        r: &Reference,
        digest: &str,
        out: &Path,
    ) -> Result<(), RegistryError> {
        let url = self.blob_url(r, digest);
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(RegistryError::Status(url, resp.status()));
        }
        let mut file = tokio::fs::File::create(out).await?;
        let mut hasher = Sha256::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        let got = format!("sha256:{}", hex::encode(hasher.finalize()));
        if !digest.eq_ignore_ascii_case(&got) {
            let _ = tokio::fs::remove_file(out).await;
            return Err(RegistryError::DigestMismatch {
                reference: r.repository.clone(),
                expected: digest.to_string(),
                got,
            });
        }
        Ok(())
    }
}
