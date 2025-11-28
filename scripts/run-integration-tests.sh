#!/usr/bin/env bash
set -euo pipefail

CLUSTER_NAME="${CLUSTER_NAME:-pvc-reaper-test}"
CLEANUP="${CLEANUP:-true}"

echo "Running integration tests..."
echo ""

# Check if kind is installed
if ! command -v kind &> /dev/null; then
    echo "Error: kind is not installed. Please install it first:"
    echo "  https://kind.sigs.k8s.io/docs/user/quick-start/#installation"
    exit 1
fi

# Check if cluster exists
if ! kind get clusters | grep -q "^${CLUSTER_NAME}$"; then
    echo "Cluster '${CLUSTER_NAME}' does not exist. Creating it..."
    ./scripts/setup-test-cluster.sh
fi

# Export kubeconfig
export KUBECONFIG
KUBECONFIG=$(kind get kubeconfig-path --name "${CLUSTER_NAME}" 2>/dev/null || echo "$HOME/.kube/kind-${CLUSTER_NAME}-config")
kind export kubeconfig --name "${CLUSTER_NAME}"

echo "Using cluster: ${CLUSTER_NAME}"
echo "Kubeconfig: ${KUBECONFIG}"
echo ""

# Verify cluster is accessible
if ! kubectl cluster-info &> /dev/null; then
    echo "Error: Cannot connect to cluster. Please check your kubeconfig."
    exit 1
fi

echo "Cluster info:"
kubectl cluster-info
echo ""

# Run unit tests first
echo "Running unit tests..."
cargo test --lib
echo ""

# Run integration tests
echo "Running integration tests..."
cargo test --test integration_test -- --ignored --test-threads=1

TEST_EXIT_CODE=$?

if [ "$TEST_EXIT_CODE" -eq 0 ]; then
    echo ""
    echo "✓ All tests passed!"
else
    echo ""
    echo "✗ Some tests failed!"
fi

# Cleanup if requested
if [ "${CLEANUP}" = "true" ] && [ "$TEST_EXIT_CODE" -eq 0 ]; then
    echo ""
    read -p "Do you want to delete the test cluster? (y/N): " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        echo "Deleting cluster..."
        kind delete cluster --name "${CLUSTER_NAME}"
        echo "Cluster deleted"
    fi
fi

exit $TEST_EXIT_CODE
