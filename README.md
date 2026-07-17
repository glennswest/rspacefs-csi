# rspacefs-csi

**Kubernetes / OpenShift CSI driver for [rspacefs](https://github.com/glennswest/rspacefs) data PVCs — in Rust.**

rspacefs is a **layered filesystem** (userspace LayerFS: a writable upper over
N read-only lower layers, OCI whiteouts, copy-up on write), served through
FUSE. An rspacefs PVC is that layer stack with the lowers seeded from an OCI
registry and the upper writable — a **semi-portable, single-consumer data
volume**: one pod uses it at a time, it can be handed off to a new container
generation, and its contents can be captured back into a registry artifact for
the next boot to start from.

This repo is the CSI driver that makes those PVCs first-class Kubernetes
objects. It is **not block storage** — there is no LUN, no attach, no
replication. Every CSI operation is *mount-shaped*: the node plugin FUSE-mounts
a composed layer view; there is nothing to format or attach.

> **Status: scaffolding.** The design is settled (below); the gRPC service
> bodies are stubbed. See [issue tracker](https://github.com/glennswest/rspacefs-csi/issues)
> for progress.

## Where this sits (platform storage taxonomy)

Distinct PVC providers, distinct engines — pick by StorageClass:

| Provider | Semantics | Pick it for |
|---|---|---|
| **rspacefs** (this driver) | Layered-FS data PVC — registry-seeded lowers + writable upper, one consumer at a time, hand-off-able to new containers and migratable with them; *not* a shared filesystem | Fork/spawn workflows, passing a data volume between container generations, boot-from-registry initial content |
| **StormBlock** (`stormblock-csi`) | RWO **block**, wandering replicated master/slave pair, master local to the pod | Data that must survive node death with automatic fenced failover |
| **StormFS** | True shared read-write (RWX) filesystem | Genuinely concurrent multi-pod access |

The three share only the generic CSI gRPC skeleton and manifest shapes — the
volume lifecycle, the attach/mount path, and the snapshot mechanism are
entirely different per provider. rspacefs-csi is deliberately its own driver
and repo; it drives [`rspacefs-mount --pvc`](https://github.com/glennswest/rspacefs/blob/main/docs/pvc.md)
and shares nothing with StormBlock's block/replication machinery.

## How it maps to CSI

The driver is one binary run as the usual two Kubernetes components. Because
rspacefs is a filesystem, not a block device, the RPCs land differently than a
block CSI driver:

| CSI RPC | rspacefs-csi behavior |
|---|---|
| `CreateVolume` | Resolve the seed OCI artifact(s) from StorageClass/PVC params (e.g. `seed: qregistry.local/pvcs/db:rev1`), pull the blob(s), allocate the writable upper dir. The "volume" is a layer stack, not a provisioned block extent. |
| `DeleteVolume` | Optionally `capture-layer` the upper to a registry artifact (capture-on-delete policy), then release the upper. |
| `NodeStageVolume` / `NodeUnstageVolume` | Largely no-ops — there is no block device to format or attach. |
| `NodePublishVolume` | Exec `rspacefs-mount --pvc --lower-blob <pulled> --upper <dir> --owner <uid:gid> --control-socket <sock> <target_path>` — a FUSE **mount** of the merged layer view. |
| `NodeUnpublishVolume` | Unmount and reap the daemon (via the control socket / signal). |
| `CreateSnapshot` | `capture-layer` → a deterministic tar+zstd OCI artifact, deduped by digest. A snapshot **is** a pushable/pullable registry revision — that's the whole point. |
| Identity | Advertises driver name `rspacefs.csi.g8.io`, CSI spec v1.x. |

## Components

```
                 ┌─────────────────────────────────────────────┐
                 │  Controller plugin (Deployment)              │
   PVC create ──►│  CreateVolume / DeleteVolume / *Snapshot     │
                 │  → pull/push OCI layer artifacts             │
                 └──────────────────────┬──────────────────────┘
                                        │
                 ┌──────────────────────▼──────────────────────┐
   pod bind ────►│  Node plugin (DaemonSet, one per node)       │
                 │  NodePublishVolume → rspacefs-mount --pvc    │
                 │  → FUSE mount of upper + registry-seeded      │
                 │    lower layers at the kubelet target path   │
                 └──────────────────────┬──────────────────────┘
                                        │
                          rspacefs-mount --pvc  (the FUSE daemon)
                                        │
                    LayerFS: writable upper + N lower layers
```

## StorageClass parameters

Map 1:1 onto `rspacefs-mount --pvc` flags:

| Parameter | Maps to | Notes |
|---|---|---|
| `seed` | `--lower-blob` (0..N) | OCI artifact ref(s) pulled as read-only lower layers. Zero = empty PVC. |
| `accessMode` | `--access-mode` | `empty` \| `ro` \| `rwo` \| `rwx` |
| `lifecycle` | `--lifecycle` | `persistent` \| `ephemeral` \| `ephemeral-then-persistent` |
| `owner` | `--owner UID:GID` | The workload's runAsUser. |
| `captureOnDelete` | `capture-layer` at `DeleteVolume` | Push the final upper as a new registry revision. |

## Building

Rust workspace; Linux target (the node plugin runs where `rspacefs-mount`
does). Follows the same build-host convention as rspacefs.

```sh
cargo build --workspace --release
make image        # container image for the DaemonSet / Deployment
```

## Related projects

- [**rspacefs**](https://github.com/glennswest/rspacefs) — the layered filesystem + `rspacefs-mount --pvc` this driver drives. See [`docs/pvc.md`](https://github.com/glennswest/rspacefs/blob/main/docs/pvc.md).
- [**rspaced**](https://github.com/glennswest/rspaced) — node/boot agent; the non-CSI owner of PVC mounts today.
- [**rspace_registry**](https://github.com/glennswest/rspace_registry) — OCI registry head that stores PVC layer revisions.

## License

MIT — see [LICENSE](LICENSE).
