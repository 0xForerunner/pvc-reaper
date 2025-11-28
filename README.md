# PVC Reaper

A Kubernetes controller that automatically deletes PersistentVolumeClaims (PVCs) pointing to nodes that no longer exist. Built in Rust for performance and reliability.

## Overview

PVC Reaper is designed primarily for OpenEBS workflows that use ephemeral local NVMe disks on cloud infrastructure. When nodes are recycled or terminated, PVCs can become orphaned, preventing pods from being rescheduled. PVC Reaper solves this by:

1. **Detecting Missing Nodes**: Automatically identifies PVCs that reference nodes that no longer exist in the cluster
2. **Handling Pending Pods**: Detects pods stuck in a pending state because their PVC is bound to a missing or unavailable node
3. **Automatic Cleanup**: Safely deletes orphaned PVCs, allowing Kubernetes to reschedule pods with new PVCs

## Features

- Continuous reconciliation loop for automatic detection and cleanup
- Configurable storage class and provisioner filtering
- Dry-run mode for testing
- Pending pod detection with configurable thresholds
- Comprehensive logging with structured output
- Low resource footprint
- Secure by default (runs as non-root, read-only root filesystem)
- Native Kubernetes deployment via Helm

## Installation

### Prerequisites

- Kubernetes cluster (1.24+)
- Helm 3.x
- Appropriate RBAC permissions (ClusterRole for PVC, Node, Pod operations)

### Using Helm

```bash
# Add the Helm repository (update with your actual repository)
helm repo add pvc-reaper https://your-org.github.io/pvc-reaper
helm repo update

# Install with default configuration
helm install pvc-reaper pvc-reaper/pvc-reaper \
  --namespace pvc-reaper \
  --create-namespace

# Install with custom configuration
helm install pvc-reaper pvc-reaper/pvc-reaper \
  --namespace pvc-reaper \
  --create-namespace \
  --set config.storageClassNames="openebs-lvm,local-storage" \
  --set config.reconcileIntervalSecs=30 \
  --set config.dryRun=true
```

### From Source

```bash
# Clone the repository
git clone https://github.com/your-org/pvc-reaper.git
cd pvc-reaper

# Install using local Helm chart
helm install pvc-reaper ./helm/pvc-reaper \
  --namespace pvc-reaper \
  --create-namespace
```

## Configuration

PVC Reaper can be configured through Helm values or environment variables:

| Helm Value | Environment Variable | Default | Description |
|------------|---------------------|---------|-------------|
| `config.storageClassNames` | `STORAGE_CLASS_NAMES` | `openebs-lvm` | Comma-separated list of storage classes to monitor |
| `config.storageProvisioner` | `STORAGE_PROVISIONER` | `local.csi.openebs.io` | Storage provisioner annotation to filter PVCs |
| `config.reconcileIntervalSecs` | `RECONCILE_INTERVAL_SECS` | `60` | Seconds between reconciliation loops |
| `config.dryRun` | `DRY_RUN` | `false` | If true, log actions without deleting PVCs |
| `config.checkPendingPods` | `CHECK_PENDING_PODS` | `true` | Enable pending pod detection |
| `config.pendingPodThresholdSecs` | `PENDING_POD_THRESHOLD_SECS` | `300` | Seconds a pod must be pending before action |
| `logLevel` | `RUST_LOG` | `info` | Log level (trace, debug, info, warn, error) |

### Example Configuration

```yaml
# values.yaml
config:
  storageClassNames: "openebs-lvm,local-storage"
  storageProvisioner: "local.csi.openebs.io"
  reconcileIntervalSecs: 30
  dryRun: false
  checkPendingPods: true
  pendingPodThresholdSecs: 300

logLevel: info

resources:
  limits:
    cpu: 200m
    memory: 256Mi
  requests:
    cpu: 100m
    memory: 128Mi
```

## How It Works

### PVC Detection and Deletion

PVC Reaper monitors PersistentVolumeClaims across all namespaces and:

1. Filters PVCs by storage class and provisioner annotation
2. Checks the `volume.kubernetes.io/selected-node` annotation
3. Verifies if the referenced node exists in the cluster
4. Deletes PVCs that reference non-existent nodes

### Pending Pod Detection

When enabled, PVC Reaper also:

1. Scans for pods in the `Pending` state
2. Checks if pods have been pending longer than the threshold
3. Examines PVCs referenced by pending pods
4. Deletes PVCs that reference missing nodes, allowing pods to reschedule

## Use Cases

### OpenEBS Local PV

PVC Reaper is ideal for OpenEBS Local PV workflows where:
- Storage is ephemeral (local NVMe disks)
- Nodes are frequently recycled (spot instances, autoscaling)
- PVCs need to be recreated on new nodes

### Cloud Autoscaling

In cloud environments with node autoscaling:
- Nodes can be terminated without warning
- PVCs can become orphaned
- Pods fail to reschedule due to node affinity

### Stateless Workloads with Local Storage

For workloads that:
- Use local storage for caching or temporary data
- Can tolerate data loss
- Need automatic recovery when nodes fail

## Monitoring

### Logs

View logs to monitor PVC Reaper activity:

```bash
kubectl logs -f deployment/pvc-reaper -n pvc-reaper
```

Example log output:

```
INFO pvc_reaper: Starting pvc-reaper
INFO pvc_reaper: Storage class names: openebs-lvm
INFO pvc_reaper: Reconcile interval: 60s
INFO pvc_reaper: Starting reconciliation cycle
INFO pvc_reaper: Found 3 available nodes
INFO pvc_reaper: PVC default/data-pod-abc references missing node 'node-xyz' - marking for deletion
INFO pvc_reaper: Successfully deleted PVC default/data-pod-abc
INFO pvc_reaper: Reconciliation complete: deleted=1, skipped=5
```

### Metrics

Currently, PVC Reaper uses structured logging. Future versions may include Prometheus metrics.

## Safety Features

- **Dry Run Mode**: Test behavior without making changes
- **Configurable Thresholds**: Avoid premature deletion of PVCs
- **Non-root Execution**: Runs as user 1000 with minimal permissions
- **Read-only Root Filesystem**: Enhanced container security
- **Precise Filtering**: Only affects PVCs matching specific criteria

## Development

### Prerequisites

This project uses [just](https://just.systems) as a command runner. Install it first:

```bash
# macOS
brew install just

# Linux/Windows/Other - see INSTALL_JUST.md
cargo install just
```

### Building from Source

```bash
# Build the binary
just build

# Run unit tests
just test

# Run integration tests (requires Docker)
just test-integration

# Run all tests
just test-all

# Build Docker image
just docker-build

# Run all checks (format, lint, test)
just check
```

### Running Locally

```bash
# Run with debug logging
just dev

# Or with cargo directly
RUST_LOG=debug cargo run -- \
  --storage-class-names openebs-lvm \
  --dry-run true
```

### Project Structure

```
pvc-reaper/
├── src/
│   ├── lib.rs            # Core logic library
│   └── main.rs           # CLI application
├── helm/
│   └── pvc-reaper/       # Helm chart
├── tests/
│   └── integration_test.rs  # Integration tests with testcontainers
├── scripts/              # Helper scripts
├── Cargo.toml            # Rust dependencies
├── Dockerfile            # Multi-stage Docker build
├── justfile             # Build automation with just
├── TESTING.md           # Comprehensive testing guide
└── README.md
```

### Testing

PVC Reaper includes comprehensive tests:

- **Unit Tests** - Fast, isolated tests of core logic (7 tests)
- **Integration Tests** - Full end-to-end tests using k3s via testcontainers (5 tests)

```bash
# Run unit tests (fast, no Docker required)
just test

# Run integration tests (requires Docker)
just test-integration

# Run all tests
just test-all
```

Integration tests use **testcontainers** to automatically spin up a k3s Kubernetes cluster, making tests completely self-contained and portable. No external tools like Kind or minikube are required.

See [TESTING.md](TESTING.md) for detailed testing documentation.

## CI/CD

GitHub Actions workflows are provided for:
- Building and testing on push
- Publishing Docker images
- Publishing Helm charts

## Troubleshooting

### PVCs Not Being Deleted

1. **Check logs** for any errors or warnings
2. **Verify RBAC** permissions are correctly set
3. **Confirm storage class** matches configuration
4. **Check annotations** on PVCs (`volume.kubernetes.io/selected-node`)
5. **Try dry-run mode** to see what would be deleted

### Unwanted Deletions

1. **Review configuration** to ensure correct storage classes
2. **Increase pending pod threshold** to avoid premature deletion
3. **Use dry-run mode** first to validate behavior
4. **Check node labels** and ensure nodes are correctly registered

## Contributing

Contributions are welcome! Please:
1. Fork the repository
2. Create a feature branch
3. Make your changes with tests
4. Submit a pull request

## License

MIT License - see LICENSE file for details

## Support

- **Issues**: Report bugs or request features on GitHub Issues
- **Documentation**: See [GitHub Wiki](https://github.com/your-org/pvc-reaper/wiki)

## Comparison with CronJob Approach

Unlike the CronJob-based approach, PVC Reaper:
- **Runs continuously** with configurable intervals
- **More efficient** (single pod vs. multiple job pods)
- **Better resource usage** (Rust vs. bash + kubectl)
- **Enhanced features** (pending pod detection, dry-run mode)
- **Structured logging** for better observability
- **Type-safe** Kubernetes API interactions
