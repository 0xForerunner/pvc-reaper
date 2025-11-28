# Quick Start Guide

This guide will help you get PVC Reaper up and running quickly.

## Prerequisites

- Kubernetes cluster (1.24+)
- Helm 3.x installed
- `kubectl` configured to access your cluster

## Installation

### 1. Quick Install with Helm

```bash
# Install with defaults (will run in dry-run mode to be safe)
helm install pvc-reaper ./helm/pvc-reaper \
  --namespace pvc-reaper \
  --create-namespace \
  --set config.dryRun=true
```

### 2. Verify Installation

```bash
# Check that the pod is running
kubectl get pods -n pvc-reaper

# View logs
kubectl logs -f -n pvc-reaper deployment/pvc-reaper
```

### 3. Test in Dry-Run Mode

The initial installation runs in dry-run mode, which means it will:
- Log what it would delete
- Not actually delete any PVCs

Monitor the logs to ensure it's detecting PVCs correctly:

```bash
kubectl logs -f -n pvc-reaper deployment/pvc-reaper
```

### 4. Enable Actual Deletion

Once you're comfortable with the behavior, disable dry-run mode:

```bash
helm upgrade pvc-reaper ./helm/pvc-reaper \
  --namespace pvc-reaper \
  --set config.dryRun=false
```

## Configuration Examples

### Example 1: Multiple Storage Classes

```bash
helm install pvc-reaper ./helm/pvc-reaper \
  --namespace pvc-reaper \
  --create-namespace \
  --set config.storageClassNames="openebs-lvm,local-storage,ebs-sc"
```

### Example 2: Custom Reconcile Interval

```bash
helm install pvc-reaper ./helm/pvc-reaper \
  --namespace pvc-reaper \
  --create-namespace \
  --set config.reconcileIntervalSecs=30 \
  --set config.pendingPodThresholdSecs=600
```

### Example 3: Disable Pending Pod Checks

```bash
helm install pvc-reaper ./helm/pvc-reaper \
  --namespace pvc-reaper \
  --create-namespace \
  --set config.checkPendingPods=false
```

## Building from Source

### Build the Binary

```bash
# Install Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build the project
cargo build --release

# Run locally (requires kubeconfig)
./target/release/pvc-reaper --dry-run true
```

### Build Docker Image

```bash
# Build the image
docker build -t pvc-reaper:dev .

# Test the image
docker run --rm pvc-reaper:dev --help
```

### Deploy Custom Image

```bash
helm install pvc-reaper ./helm/pvc-reaper \
  --namespace pvc-reaper \
  --create-namespace \
  --set image.repository=pvc-reaper \
  --set image.tag=dev \
  --set image.pullPolicy=Never
```

## Testing the Behavior

### Create a Test PVC with Missing Node

```bash
# Create a test namespace
kubectl create namespace pvc-test

# Create a PVC with a fake selected-node annotation
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: test-pvc
  namespace: pvc-test
  annotations:
    volume.kubernetes.io/selected-node: "fake-node-that-does-not-exist"
    volume.beta.kubernetes.io/storage-provisioner: "local.csi.openebs.io"
spec:
  accessModes:
    - ReadWriteOnce
  storageClassName: openebs-lvm
  resources:
    requests:
      storage: 1Gi
EOF

# Check if PVC Reaper detects it
kubectl logs -f -n pvc-reaper deployment/pvc-reaper | grep test-pvc
```

## Monitoring

### View Logs

```bash
# Follow logs
kubectl logs -f -n pvc-reaper deployment/pvc-reaper

# View recent logs
kubectl logs -n pvc-reaper deployment/pvc-reaper --tail=100

# View logs with timestamps
kubectl logs -n pvc-reaper deployment/pvc-reaper --timestamps
```

### Check Status

```bash
# Check deployment status
kubectl get deployment -n pvc-reaper

# Describe the pod
kubectl describe pod -n pvc-reaper -l app.kubernetes.io/name=pvc-reaper

# Check RBAC permissions
kubectl get clusterrole pvc-reaper-pvc-reaper -o yaml
kubectl get clusterrolebinding pvc-reaper-pvc-reaper -o yaml
```

## Troubleshooting

### PVC Reaper Not Starting

```bash
# Check pod status
kubectl get pods -n pvc-reaper

# Check pod events
kubectl describe pod -n pvc-reaper -l app.kubernetes.io/name=pvc-reaper

# Check logs for errors
kubectl logs -n pvc-reaper deployment/pvc-reaper
```

### PVCs Not Being Deleted

1. Verify storage class matches configuration
2. Check PVC annotations
3. Ensure node is actually missing
4. Check RBAC permissions
5. Look for errors in logs

### Unwanted Deletions in Dry-Run

If you see PVCs being flagged for deletion that shouldn't be:
1. Review your storage class configuration
2. Check the node names are correct
3. Verify annotations on PVCs

## Uninstallation

```bash
# Remove the Helm release
helm uninstall pvc-reaper -n pvc-reaper

# Remove the namespace
kubectl delete namespace pvc-reaper

# Remove ClusterRole and ClusterRoleBinding (if not auto-deleted)
kubectl delete clusterrole pvc-reaper-pvc-reaper
kubectl delete clusterrolebinding pvc-reaper-pvc-reaper
```

## Next Steps

- Read the full [README.md](README.md) for detailed information
- Check [CONTRIBUTING.md](CONTRIBUTING.md) if you want to contribute
- Review the [Helm chart values](helm/pvc-reaper/values.yaml) for all configuration options
