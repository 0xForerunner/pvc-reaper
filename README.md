# PVC Reaper

![PVC Reaper](docs/assets/pvc-reaper.png)

A Rust-powered Kubernetes controller that reaps PersistentVolumeClaims (PVCs) referencing nodes that no longer exist. It keeps OpenEBS and other local-storage workloads healthy by automatically clearing orphaned PVCs so pods can be rescheduled immediately.

## Overview

PVC Reaper is designed primarily for OpenEBS workflows that use ephemeral local NVMe disks on cloud infrastructure. When nodes are recycled or terminated, PVCs and their backing PV metadata can become orphaned, preventing pods from being rescheduled or leaving storage objects stuck in `Terminating`. PVC Reaper solves this by:

1. **Detecting Missing Nodes**: Automatically identifies PVCs that reference nodes that no longer exist in the cluster
2. **Handling Unschedulable Pods**: Detects pods stuck in an unschedulable state because their PVC is bound to a missing or unavailable node
3. **Automatic PVC Cleanup**: Safely deletes orphaned PVCs, allowing Kubernetes to reschedule pods with new PVCs
4. **Stale PV Cleanup**: Clears finalizers from released local PVs and matching OpenEBS `LVMVolume` objects when their node is gone and Kubernetes/OpenEBS can no longer complete deletion

## Features

- Continuous reaping loop that watches all namespaces
- Configurable storage class and provisioner filters
- Optional unschedulable pod detection with configurable thresholds
- Stale PV and OpenEBS `LVMVolume` candidate logging even when cleanup is disabled
- Dry-run mode that logs intended actions without modifying Kubernetes resources
- Lightweight, non-root container with a read-only root filesystem
- Deployable with the included Helm chart or prebuilt container image

## Installation

PVC Reaper expects Kubernetes 1.24+, Helm 3, and RBAC permissions allowing PVC, PV, Pod, Node, and OpenEBS `LVMVolume` access.

### Helm (recommended)


##### Install the chart

```bash
helm upgrade --install pvc-reaper oci://ghcr.io/0xforerunner/charts/pvc-reaper --namespace pvc-reaper --create-namespace
```

## Configuration

Tune the controller via Helm values or the matching environment variables:

| Helm Value | Env Var | Default | Description |
|------------|---------|---------|-------------|
| `config.storageClassNames` | `STORAGE_CLASS_NAMES` | `openebs-lvm` | Comma-separated list of storage classes to watch |
| `config.storageProvisioner` | `STORAGE_PROVISIONER` | `local.csi.openebs.io` | Provisioner annotation used to filter PVCs |
| `config.reapIntervalSecs` | `REAP_INTERVAL_SECS` | `60` | Seconds between reaping loops |
| `config.dryRun` | `DRY_RUN` | `false` | Log actions without deleting PVCs, patching PVs, or deleting/patching `LVMVolume` objects |
| `config.cleanupPvcs` | `CLEANUP_PVCS` | `true` | Delete PVC cleanup candidates when not in dry-run mode |
| `config.checkUnschedulablePods` | `CHECK_UNSCHEDULABLE_PODS` | `true` | Enable unschedulable pod scanning |
| `config.unschedulablePodThresholdSecs` | `UNSCHEDULABLE_POD_THRESHOLD_SECS` | `120` | How long a pod must be unschedulable before action |
| `config.cleanupPvs` | `CLEANUP_PVS` | `true` | Enable cleanup for released local PVs whose node is gone; candidates are logged either way |
| `config.pvGracePeriodSecs` | `PV_GRACE_PERIOD_SECS` | `600` | How long a PV must be released/deleting before action |
| `config.cleanupOpenebsLvmVolumes` | `CLEANUP_OPENEBS_LVMVOLUMES` | `true` | Delete matching OpenEBS `LVMVolume` objects and clear their finalizers; candidates are logged either way |
| `config.openebsNamespace` | `OPENEBS_NAMESPACE` | `openebs` | Namespace containing OpenEBS `LVMVolume` objects |
| `logLevel` | `RUST_LOG` | `info` | Controller log level |

Minimal values example:

```yaml
config:
  storageClassNames: "openebs-lvm,local-storage"
  storageProvisioner: "local.csi.openebs.io"
  reapIntervalSecs: 30
  dryRun: false
  cleanupPvcs: true
  checkUnschedulablePods: true
  unschedulablePodThresholdSecs: 120
  cleanupPvs: true
  pvGracePeriodSecs: 600
  cleanupOpenebsLvmVolumes: true
  openebsNamespace: openebs
logLevel: info
```

## How it works

1. PVC Reaper filters PVCs based on the configured storage classes/provisioners.
2. For each PVC it inspects the `volume.kubernetes.io/selected-node` annotation.
3. If the referenced node no longer exists, the PVC is deleted when `cleanupPvcs` is enabled, or only logged when cleanup is disabled or dry-run mode is enabled.
4. Optional unschedulable pod detection scans pods stuck in `Unschedulable`, inspects their PVCs, and reaps any that reference missing nodes so workloads can be rescheduled with fresh storage.
5. PV cleanup scans released matching PVs. If the referenced claim UID is gone, the reclaim policy is `Delete`, the PV points at a missing node, and the grace period has elapsed, the controller logs the PV and matching OpenEBS `LVMVolume` actions it would take. When cleanup is enabled and dry-run mode is disabled, it clears PV finalizers and cleans matching OpenEBS `LVMVolume` objects so deletion can finish.

## Development

This repo uses [just](https://just.systems) to keep commands short:

```bash
just build          # Compile the controller
just test           # Run unit tests
just dev            # Start a local dev build with debug logs
```

## Contributing

Issues and pull requests are welcome. Please fork the repo, create a feature branch, and include tests or reproduction steps where possible.

## License

MIT – see `LICENSE` for details.
