#!/usr/bin/env bash
set -euo pipefail

CLUSTER_NAME="${CLUSTER_NAME:-pvc-reaper-test}"

echo "Setting up test Kubernetes cluster using Kind..."

# Check if kind is installed
if ! command -v kind &> /dev/null; then
    echo "Error: kind is not installed. Please install it first:"
    echo "  https://kind.sigs.k8s.io/docs/user/quick-start/#installation"
    exit 1
fi

# Check if cluster already exists
if kind get clusters | grep -q "^${CLUSTER_NAME}$"; then
    echo "Cluster '${CLUSTER_NAME}' already exists"
    read -p "Do you want to delete and recreate it? (y/N): " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        echo "Deleting existing cluster..."
        kind delete cluster --name "${CLUSTER_NAME}"
    else
        echo "Using existing cluster"
        kind export kubeconfig --name "${CLUSTER_NAME}"
        exit 0
    fi
fi

# Create kind cluster
echo "Creating Kind cluster '${CLUSTER_NAME}'..."
cat <<EOF | kind create cluster --name "${CLUSTER_NAME}" --config=-
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
- role: control-plane
- role: worker
- role: worker
EOF

echo ""
echo "Cluster created successfully!"
echo ""
echo "To use this cluster:"
echo "  export KUBECONFIG=\$(kind get kubeconfig --name ${CLUSTER_NAME})"
echo ""
echo "To delete this cluster:"
echo "  kind delete cluster --name ${CLUSTER_NAME}"
echo ""
echo "Ready to run integration tests:"
echo "  cargo test --test integration_test -- --ignored"
