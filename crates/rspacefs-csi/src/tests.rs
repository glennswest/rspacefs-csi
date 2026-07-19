//! Unit tests for the driver logic, run against in-memory mock backends so the
//! whole flow exercises off-cluster (no real registry, FUSE mount, or daemon).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use csi_proto::controller_server::Controller;
use csi_proto::node_server::Node;
use tonic::Request;

use crate::control::{CaptureData, Capturer, ControlError};
use crate::controller::ControllerService;
use crate::driver::Config;
use crate::mount::{MountError, MountSpec, Mounter};
use crate::node::{sanitize_tag, NodeService};
use crate::params::{AccessMode, Lifecycle, PvcParams};
use crate::registry::{LayerRef, Reference, Registry, RegistryError};

// ---- mock backends ----------------------------------------------------------

#[derive(Default)]
struct MockMounter {
    mounts: Mutex<Vec<MountSpec>>,
    unmounts: Mutex<Vec<PathBuf>>,
}

#[async_trait]
impl Mounter for MockMounter {
    async fn mount(&self, spec: &MountSpec) -> Result<(), MountError> {
        self.mounts.lock().unwrap().push(spec.clone());
        Ok(())
    }
    async fn unmount(&self, target: &Path) -> Result<(), MountError> {
        self.unmounts.lock().unwrap().push(target.to_path_buf());
        Ok(())
    }
}

#[derive(Default)]
struct MockRegistry {
    pulled: Mutex<Vec<String>>,
    pushed: Mutex<Vec<(String, String)>>, // (target_ref, layer_digest)
}

#[async_trait]
impl Registry for MockRegistry {
    async fn resolve(&self, _seed: &str) -> Result<Vec<LayerRef>, RegistryError> {
        Ok(vec![LayerRef {
            media_type: crate::registry::LAYER_ZSTD.into(),
            digest: "sha256:aaaa".into(),
            size: 4,
        }])
    }
    async fn pull_layers(&self, seed: &str, dir: &Path) -> Result<Vec<PathBuf>, RegistryError> {
        self.pulled.lock().unwrap().push(seed.to_string());
        tokio::fs::create_dir_all(dir).await?;
        let p = dir.join(format!("{}.blob", seed.replace(['/', ':', '@'], "-")));
        tokio::fs::write(&p, b"blob").await?;
        Ok(vec![p])
    }
    async fn push_revision(
        &self,
        target_ref: &str,
        _blob_path: &Path,
        layer_digest: &str,
        _layer_size: u64,
    ) -> Result<String, RegistryError> {
        self.pushed
            .lock()
            .unwrap()
            .push((target_ref.to_string(), layer_digest.to_string()));
        Ok(format!("{target_ref}@{layer_digest}"))
    }
}

#[derive(Default)]
struct MockCapturer {
    captures: Mutex<Vec<PathBuf>>,
}

#[async_trait]
impl Capturer for MockCapturer {
    async fn capture(&self, _socket: &Path, out_path: &Path) -> Result<CaptureData, ControlError> {
        self.captures.lock().unwrap().push(out_path.to_path_buf());
        Ok(CaptureData {
            out_path: out_path.display().to_string(),
            digest: "sha256:cafe".into(),
            bytes_compressed: 4,
            entries: 1,
        })
    }
}

fn cfg(data_dir: PathBuf) -> Arc<Config> {
    Arc::new(Config {
        node_id: "node-a".into(),
        data_dir,
        capture_registry: Some("qregistry.local".into()),
    })
}

// ---- params -----------------------------------------------------------------

#[test]
fn params_parse_full() {
    let mut m = HashMap::new();
    m.insert(
        "seed".into(),
        "qregistry.local/pvcs/db:rev1, qregistry.local/pvcs/base:v2".into(),
    );
    m.insert("accessMode".into(), "rwo".into());
    m.insert("lifecycle".into(), "ephemeral-then-persistent".into());
    m.insert("owner".into(), "1000:1000".into());
    m.insert("captureOnDelete".into(), "true".into());
    let p = PvcParams::parse(&m).unwrap();
    assert_eq!(p.seeds.len(), 2);
    assert_eq!(p.access_mode, Some(AccessMode::Rwo));
    assert_eq!(p.lifecycle, Some(Lifecycle::EphemeralThenPersistent));
    assert_eq!(p.owner.as_deref(), Some("1000:1000"));
    assert!(p.capture_on_delete);
}

#[test]
fn params_reject_bad_access_mode() {
    let mut m = HashMap::new();
    m.insert("accessMode".into(), "bogus".into());
    assert!(PvcParams::parse(&m).is_err());
}

#[test]
fn capture_repo_derived_from_seed() {
    let mut m = HashMap::new();
    m.insert("seed".into(), "qregistry.local/pvcs/db:rev1".into());
    let p = PvcParams::parse(&m).unwrap();
    assert_eq!(
        p.resolved_capture_repo().as_deref(),
        Some("qregistry.local/pvcs/db")
    );
}

#[test]
fn capture_repo_explicit_wins() {
    let mut m = HashMap::new();
    m.insert("seed".into(), "qregistry.local/pvcs/db:rev1".into());
    m.insert("captureRepo".into(), "qregistry.local/snaps/db".into());
    let p = PvcParams::parse(&m).unwrap();
    assert_eq!(
        p.resolved_capture_repo().as_deref(),
        Some("qregistry.local/snaps/db")
    );
}

// ---- reference parsing ------------------------------------------------------

#[test]
fn reference_parse_variants() {
    let r = Reference::parse("qregistry.local/pvcs/db:rev1").unwrap();
    assert_eq!(r.registry, "qregistry.local");
    assert_eq!(r.repository, "pvcs/db");
    assert_eq!(r.reference, "rev1");

    let d = format!("sha256:{}", "a".repeat(64));
    let r = Reference::parse(&format!("qregistry.local/pvcs/db@{d}")).unwrap();
    assert_eq!(r.repository, "pvcs/db");
    assert_eq!(r.reference, d);

    let r = Reference::parse("reg.local:5000/team/app").unwrap();
    assert_eq!(r.registry, "reg.local:5000");
    assert_eq!(r.reference, "latest");

    assert!(Reference::parse("no-registry-host").is_err());
}

// ---- mount arg rendering ----------------------------------------------------

#[test]
fn mount_spec_renders_expected_argv() {
    let spec = MountSpec {
        name: "db".into(),
        target: "/mnt/db".into(),
        upper: "/data/db/upper".into(),
        control_socket: "/data/db/control.sock".into(),
        access_mode: AccessMode::Ro,
        lifecycle: Lifecycle::Persistent,
        owner: Some("1000:1000".into()),
        lower_blobs: vec!["/data/db/lowers/a.blob".into()],
    };
    let args = spec.args();
    let joined = args.join(" ");
    assert_eq!(
        joined,
        "--pvc --name db --access-mode ro --lifecycle persistent \
         --lower-blob /data/db/lowers/a.blob --upper /data/db/upper \
         --owner 1000:1000 --control-socket /data/db/control.sock /mnt/db"
    );
    // No --read-only flag exists in the rspacefs-mount CLI.
    assert!(!joined.contains("--read-only"));
}

#[test]
fn sanitize_tag_rules() {
    assert_eq!(sanitize_tag("db-data"), "db-data");
    assert_eq!(sanitize_tag("weird/name space"), "weird-name-space");
    assert_eq!(sanitize_tag(""), "rev");
}

// ---- node publish / unpublish ----------------------------------------------

fn node(
    data_dir: PathBuf,
) -> (
    NodeService,
    Arc<MockMounter>,
    Arc<MockRegistry>,
    Arc<MockCapturer>,
) {
    let mounter = Arc::new(MockMounter::default());
    let registry = Arc::new(MockRegistry::default());
    let capturer = Arc::new(MockCapturer::default());
    let svc = NodeService::new(
        cfg(data_dir),
        mounter.clone(),
        registry.clone(),
        capturer.clone(),
    );
    (svc, mounter, registry, capturer)
}

#[tokio::test]
async fn node_publish_pulls_and_mounts() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("target");
    let (svc, mounter, registry, _cap) = node(tmp.path().join("data"));

    let mut ctx = HashMap::new();
    ctx.insert(
        "seed".to_string(),
        "qregistry.local/pvcs/db:rev1".to_string(),
    );
    ctx.insert("accessMode".to_string(), "rwo".to_string());
    ctx.insert("owner".to_string(), "1000:1000".to_string());
    ctx.insert(crate::params::VOLUME_NAME.to_string(), "db".to_string());

    let req = csi_proto::NodePublishVolumeRequest {
        volume_id: "vol-1".into(),
        target_path: target.display().to_string(),
        volume_context: ctx,
        readonly: false,
        ..Default::default()
    };
    svc.node_publish_volume(Request::new(req)).await.unwrap();

    assert_eq!(
        registry.pulled.lock().unwrap().as_slice(),
        &["qregistry.local/pvcs/db:rev1"]
    );
    let mounts = mounter.mounts.lock().unwrap();
    assert_eq!(mounts.len(), 1);
    let spec = &mounts[0];
    assert_eq!(spec.name, "db");
    assert_eq!(spec.access_mode, AccessMode::Rwo);
    assert_eq!(spec.owner.as_deref(), Some("1000:1000"));
    assert_eq!(spec.lower_blobs.len(), 1);
    assert!(target.exists(), "target dir created");
}

#[tokio::test]
async fn node_publish_readonly_forces_ro_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("target");
    let (svc, mounter, _reg, _cap) = node(tmp.path().join("data"));

    let mut ctx = HashMap::new();
    ctx.insert(
        "seed".to_string(),
        "qregistry.local/pvcs/db:rev1".to_string(),
    );
    let req = csi_proto::NodePublishVolumeRequest {
        volume_id: "vol-1".into(),
        target_path: target.display().to_string(),
        volume_context: ctx,
        readonly: true,
        ..Default::default()
    };
    svc.node_publish_volume(Request::new(req)).await.unwrap();
    assert_eq!(
        mounter.mounts.lock().unwrap()[0].access_mode,
        AccessMode::Ro
    );
}

#[tokio::test]
async fn node_unpublish_captures_when_requested() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let target = tmp.path().join("target");
    let (svc, mounter, registry, capturer) = node(data.clone());

    // Publish with captureOnDelete so the state file records it.
    let mut ctx = HashMap::new();
    ctx.insert(
        "seed".to_string(),
        "qregistry.local/pvcs/db:rev1".to_string(),
    );
    ctx.insert("captureOnDelete".to_string(), "true".to_string());
    ctx.insert(crate::params::VOLUME_NAME.to_string(), "db".to_string());
    let pubreq = csi_proto::NodePublishVolumeRequest {
        volume_id: "vol-1".into(),
        target_path: target.display().to_string(),
        volume_context: ctx,
        readonly: false,
        ..Default::default()
    };
    svc.node_publish_volume(Request::new(pubreq)).await.unwrap();

    // Simulate the live daemon's control socket so the freeze precheck passes.
    let sock = data.join("volumes").join("vol-1").join("control.sock");
    tokio::fs::create_dir_all(sock.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&sock, b"").await.unwrap();

    let unreq = csi_proto::NodeUnpublishVolumeRequest {
        volume_id: "vol-1".into(),
        target_path: target.display().to_string(),
    };
    svc.node_unpublish_volume(Request::new(unreq))
        .await
        .unwrap();

    assert_eq!(
        capturer.captures.lock().unwrap().len(),
        1,
        "captured on unpublish"
    );
    let pushed = registry.pushed.lock().unwrap();
    assert_eq!(pushed.len(), 1);
    assert_eq!(
        pushed[0].0, "qregistry.local/pvcs/db:db",
        "pushed to derived repo:tag"
    );
    assert_eq!(pushed[0].1, "sha256:cafe");
    assert_eq!(
        mounter.unmounts.lock().unwrap().len(),
        1,
        "unmounted after capture"
    );
}

#[tokio::test]
async fn node_unpublish_without_capture_flag_skips_freeze() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let target = tmp.path().join("target");
    let (svc, mounter, registry, capturer) = node(data.clone());

    let mut ctx = HashMap::new();
    ctx.insert(
        "seed".to_string(),
        "qregistry.local/pvcs/db:rev1".to_string(),
    );
    let pubreq = csi_proto::NodePublishVolumeRequest {
        volume_id: "vol-2".into(),
        target_path: target.display().to_string(),
        volume_context: ctx,
        readonly: false,
        ..Default::default()
    };
    svc.node_publish_volume(Request::new(pubreq)).await.unwrap();

    let unreq = csi_proto::NodeUnpublishVolumeRequest {
        volume_id: "vol-2".into(),
        target_path: target.display().to_string(),
    };
    svc.node_unpublish_volume(Request::new(unreq))
        .await
        .unwrap();

    assert!(capturer.captures.lock().unwrap().is_empty());
    assert!(registry.pushed.lock().unwrap().is_empty());
    assert_eq!(mounter.unmounts.lock().unwrap().len(), 1);
}

// ---- controller -------------------------------------------------------------

fn controller(data_dir: PathBuf) -> (ControllerService, Arc<MockRegistry>) {
    let registry = Arc::new(MockRegistry::default());
    let svc = ControllerService::new(
        cfg(data_dir),
        registry.clone(),
        Arc::new(MockCapturer::default()),
    );
    (svc, registry)
}

#[tokio::test]
async fn create_volume_validates_seed_and_passes_context() {
    let tmp = tempfile::tempdir().unwrap();
    let (svc, _reg) = controller(tmp.path().into());

    let mut params = HashMap::new();
    params.insert(
        "seed".to_string(),
        "qregistry.local/pvcs/db:rev1".to_string(),
    );
    params.insert("accessMode".to_string(), "rwo".to_string());

    let req = csi_proto::CreateVolumeRequest {
        name: "db".into(),
        parameters: params,
        volume_capabilities: vec![csi_proto::VolumeCapability::default()],
        ..Default::default()
    };
    let resp = svc
        .create_volume(Request::new(req))
        .await
        .unwrap()
        .into_inner();
    let vol = resp.volume.unwrap();
    assert_eq!(vol.volume_id, "db");
    assert_eq!(
        vol.volume_context
            .get(crate::params::VOLUME_NAME)
            .map(String::as_str),
        Some("db")
    );
    assert_eq!(
        vol.volume_context.get("seed").map(String::as_str),
        Some("qregistry.local/pvcs/db:rev1")
    );
}

#[tokio::test]
async fn create_volume_from_snapshot_adds_seed() {
    let tmp = tempfile::tempdir().unwrap();
    let (svc, registry) = controller(tmp.path().into());

    let snap = "qregistry.local/pvcs/db@sha256:cafe";
    let req = csi_proto::CreateVolumeRequest {
        name: "db-clone".into(),
        volume_capabilities: vec![csi_proto::VolumeCapability::default()],
        volume_content_source: Some(csi_proto::VolumeContentSource {
            r#type: Some(csi_proto::volume_content_source::Type::Snapshot(
                csi_proto::volume_content_source::SnapshotSource {
                    snapshot_id: snap.into(),
                },
            )),
        }),
        ..Default::default()
    };
    let resp = svc
        .create_volume(Request::new(req))
        .await
        .unwrap()
        .into_inner();
    // The snapshot ref was validated as a seed and threaded into the context.
    assert!(
        registry.pulled.lock().unwrap().is_empty(),
        "controller resolves, node pulls"
    );
    let vol = resp.volume.unwrap();
    assert_eq!(
        vol.volume_context.get("seed").map(String::as_str),
        Some(snap)
    );
}

// ---- real control-socket wire client ---------------------------------------

/// Drive the real `SocketControl` against a unix socket that speaks the exact
/// newline-JSON protocol from `rspacefs-fuse/src/control.rs`, validating the
/// request we send and the envelope we parse.
#[tokio::test]
async fn socket_control_capture_roundtrip() {
    use crate::control::SocketControl;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    let sock = std::env::temp_dir().join(format!("rspacefs-csi-ctl-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (rd, mut wr) = stream.into_split();
        let mut line = String::new();
        BufReader::new(rd).read_line(&mut line).await.unwrap();
        let req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(req["cmd"], "capture-layer");
        assert_eq!(req["out_path"], "/staging/out.tar.zst");
        wr.write_all(
            br#"{"ok":true,"data":{"out_path":"/staging/out.tar.zst","digest":"sha256:beef","bytes_compressed":42,"entries":7}}"#,
        )
        .await
        .unwrap();
        wr.write_all(b"\n").await.unwrap();
        wr.flush().await.unwrap();
    });

    let data = SocketControl
        .capture(&sock, Path::new("/staging/out.tar.zst"))
        .await
        .unwrap();
    assert_eq!(data.digest, "sha256:beef");
    assert_eq!(data.bytes_compressed, 42);
    assert_eq!(data.entries, 7);
    server.await.unwrap();
    let _ = std::fs::remove_file(&sock);
}

/// A daemon error envelope (`{"ok":false,"error":...}`) surfaces as `Remote`.
#[tokio::test]
async fn socket_control_surfaces_daemon_error() {
    use crate::control::SocketControl;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    let sock = std::env::temp_dir().join(format!("rspacefs-csi-err-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (rd, mut wr) = stream.into_split();
        let mut line = String::new();
        BufReader::new(rd).read_line(&mut line).await.unwrap();
        wr.write_all(br#"{"ok":false,"error":"capture-layer is only available on --pvc mounts"}"#)
            .await
            .unwrap();
        wr.write_all(b"\n").await.unwrap();
        wr.flush().await.unwrap();
    });

    let err = SocketControl
        .capture(&sock, Path::new("/x"))
        .await
        .unwrap_err();
    assert!(matches!(err, ControlError::Remote(_)));
    server.await.unwrap();
    let _ = std::fs::remove_file(&sock);
}

/// Real push→resolve→pull round-trip against a live `rspace_registry`.
/// Opt-in: set `RSPACEFS_TEST_REGISTRY=host:port` (http) and run with
/// `cargo test -- --ignored registry_roundtrip`.
#[tokio::test]
#[ignore = "requires a running rspace_registry (RSPACEFS_TEST_REGISTRY)"]
async fn registry_roundtrip() {
    use crate::registry::OciClient;
    use sha2::{Digest, Sha256};

    let host = std::env::var("RSPACEFS_TEST_REGISTRY").expect("RSPACEFS_TEST_REGISTRY");
    let client = OciClient::new("http", true).unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let blob = tmp.path().join("layer.tar.zst");
    let payload = b"rspacefs-pvc-data-artifact-roundtrip";
    tokio::fs::write(&blob, payload).await.unwrap();
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(payload)));

    let target = format!("{host}/pvcs/csitest:rev1");
    let pushed = client
        .push_revision(&target, &blob, &digest, payload.len() as u64)
        .await
        .expect("push_revision");
    assert!(pushed.contains("@sha256:"), "canonical ref: {pushed}");

    let layers = client.resolve(&target).await.expect("resolve");
    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].digest, digest);

    let pulled = client.pull_layers(&target, tmp.path()).await.expect("pull");
    assert_eq!(pulled.len(), 1);
    let got = tokio::fs::read(&pulled[0]).await.unwrap();
    assert_eq!(got, payload, "pulled bytes match pushed (digest-verified)");
}

#[tokio::test]
async fn create_snapshot_requires_live_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let (svc, _reg) = controller(tmp.path().into());
    // No control socket for the source volume → FailedPrecondition.
    let req = csi_proto::CreateSnapshotRequest {
        source_volume_id: "vol-x".into(),
        name: "snap1".into(),
        ..Default::default()
    };
    let err = svc.create_snapshot(Request::new(req)).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
}
