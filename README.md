# PVC Reaper

![PVC Reaper](docs/assets/pvc-reaper.png)

A Rust-powered Kubernetes controller that reaps PersistentVolumeClaims (PVCs) referencing nodes that no longer exist. It keeps OpenEBS and other local-storage workloads healthy by automatically clearing orphaned PVCs so pods can be rescheduled immediately.

## Overview

PVC Reaper is designed primarily for OpenEBS workflows that use ephemeral local NVMe disks on cloud infrastructure. When nodes are recycled or terminated, PVCs can become orphaned, preventing pods from being rescheduled. PVC Reaper solves this by:

1. **Detecting Missing Nodes**: Automatically identifies PVCs that reference nodes that no longer exist in the cluster
2. **Handling Unschedulable Pods**: Detects pods stuck in an unschedulable state because their PVC is bound to a missing or unavailable node
3. **Automatic Cleanup**: Safely deletes orphaned PVCs, allowing Kubernetes to reschedule pods with new PVCs

## Features

- Continuous reaping loop that watches all namespaces
- Configurable storage class and provisioner filters
- Optional unschedulable pod detection with configurable thresholds
- Dry-run mode plus structured logging for confidence in production
- Lightweight, non-root container with a read-only root filesystem
- Deployable with the included Helm chart or prebuilt container image

## Installation

PVC Reaper expects Kubernetes 1.24+, Helm 3, and RBAC permissions allowing PVC, Pod, and Node access.

### Helm (recommended)

```bash
# From the chart packaged in this repo
helm install pvc-reaper ./helm/pvc-reaper \
  --namespace pvc-reaper \
  --create-namespace

# Override values inline or with a values.yaml file
helm upgrade --install pvc-reaper ./helm/pvc-reaper \
  --namespace pvc-reaper \
  --create-namespace \
  --set config.storageClassNames="openebs-lvm,local-storage"
```

### From source

```bash
git clone https://github.com/0xforerunner/pvc-reaper.git
cd pvc-reaper
helm install pvc-reaper ./helm/pvc-reaper \
  --namespace pvc-reaper \
  --create-namespace
```

## Configuration

Tune the controller via Helm values or the matching environment variables:

| Helm Value | Env Var | Default | Description |
|------------|---------|---------|-------------|
| `config.storageClassNames` | `STORAGE_CLASS_NAMES` | `openebs-lvm` | Comma-separated list of storage classes to watch |
| `config.storageProvisioner` | `STORAGE_PROVISIONER` | `local.csi.openebs.io` | Provisioner annotation used to filter PVCs |
| `config.reapIntervalSecs` | `REAP_INTERVAL_SECS` | `60` | Seconds between reaping loops |
| `config.dryRun` | `DRY_RUN` | `false` | Log actions without deleting PVCs |
| `config.checkUnschedulablePods` | `CHECK_UNSCHEDULABLE_PODS` | `true` | Enable unschedulable pod scanning |
| `config.unschedulablePodThresholdSecs` | `UNSCHEDULABLE_POD_THRESHOLD_SECS` | `120` | How long a pod must be unschedulable before action |
| `logLevel` | `RUST_LOG` | `info` | Controller log level |

Minimal values example:

```yaml
config:
  storageClassNames: "openebs-lvm,local-storage"
  storageProvisioner: "local.csi.openebs.io"
  reapIntervalSecs: 30
  dryRun: false
  checkUnschedulablePods: true
  unschedulablePodThresholdSecs: 300
logLevel: info
```

## How it works

1. PVC Reaper filters PVCs based on the configured storage classes/provisioners.
2. For each PVC it inspects the `volume.kubernetes.io/selected-node` annotation.
3. If the referenced node no longer exists, the PVC is deleted (or logged when in dry-run mode).
4. Optional unschedulable pod detection scans pods stuck in `Unschedulable`, inspects their PVCs, and reaps any that reference missing nodes so workloads can be rescheduled with fresh storage.

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
