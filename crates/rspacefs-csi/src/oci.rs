//! OCI registry interaction for seed lowers and captured snapshots.
//!
//! This is the seam onto `rspace_registry`. For now the functions validate and
//! pass refs through; the real implementation pulls blobs into a shared content
//! store and captures uppers back as deterministic tar+zstd artifacts.

use std::io;

/// Resolve a seed OCI ref to a local read-only lower blob path.
///
/// TODO(rspacefs): pull `ref` from the registry into the shared content store
/// and return the on-disk blob path. Until then we pass the ref through so the
/// mount command is well-formed and the pull can be wired in behind this seam.
pub async fn resolve_lower(seed: &str) -> io::Result<String> {
    if seed.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty seed ref",
        ));
    }
    Ok(seed.to_string())
}

/// Deterministic snapshot revision ref for a captured upper.
///
/// TODO(rspacefs): the real ref is derived from the capture-layer digest so
/// identical uppers dedupe to the same registry revision.
pub fn snapshot_ref(source_volume_id: &str, snapshot_name: &str) -> String {
    format!("{source_volume_id}@snap-{snapshot_name}")
}
