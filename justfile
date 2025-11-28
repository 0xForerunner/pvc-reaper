# PVC Reaper - Just commands
# Run `just` or `just --list` to see all available commands

# Variables
IMAGE_NAME := env_var_or_default("IMAGE_NAME", "ghcr.io/0xforerunner/pvc-reaper")
VERSION := env_var_or_default("VERSION", "0.1.0")

# Default recipe (runs when you just type `just`)
default:
    @just --list

# Show this help message
help:
    @echo 'Usage: just <recipe>'
    @echo ''
    @echo 'Available recipes:'
    @just --list

# Build the Rust binary
build:
    cargo build --release

# Run unit tests
test:
    cargo test --lib

# Run integration tests (requires Docker, skips on macOS)
test-integration:
    cargo test --test integration_test -- --ignored --test-threads=1

# Run all tests
test-all: test test-integration

# Format code
fmt:
    cargo fmt

# Format code (check only, no changes)
fmt-check:
    cargo fmt --all -- --check

# Run clippy
lint:
    cargo clippy -- -D warnings

# Build Docker image
docker-build:
    docker build -t {{IMAGE_NAME}}:{{VERSION}} .
    docker tag {{IMAGE_NAME}}:{{VERSION}} {{IMAGE_NAME}}:latest

# Push Docker image
docker-push:
    docker push {{IMAGE_NAME}}:{{VERSION}}
    docker push {{IMAGE_NAME}}:latest

# Build and push Docker image
docker: docker-build docker-push

# Lint Helm chart
helm-lint:
    helm lint helm/pvc-reaper

# Template Helm chart
helm-template:
    helm template pvc-reaper helm/pvc-reaper --namespace pvc-reaper

# Package Helm chart
helm-package:
    mkdir -p dist
    helm package helm/pvc-reaper -d dist/

# Install Helm chart locally (dry-run by default)
helm-install:
    helm install pvc-reaper helm/pvc-reaper --namespace pvc-reaper --create-namespace

# Upgrade Helm chart
helm-upgrade:
    helm upgrade pvc-reaper helm/pvc-reaper --namespace pvc-reaper

# Uninstall Helm chart
helm-uninstall:
    helm uninstall pvc-reaper --namespace pvc-reaper

# Clean build artifacts
clean:
    cargo clean
    rm -rf dist/

# Run in development mode with debug logging
dev:
    RUST_LOG=debug cargo run

# Run in development mode with dry-run
dev-dry-run:
    RUST_LOG=debug cargo run -- --dry-run true

# Run all checks (format, lint, test, helm-lint)
check: fmt-check lint test helm-lint
    @echo "✅ All checks passed!"

# Run all checks including integration tests
check-all: fmt-check lint test-all helm-lint
    @echo "✅ All checks passed (including integration tests)!"

# Generate test coverage report
coverage:
    cargo tarpaulin --out Html --output-dir coverage
    @echo "Coverage report generated in coverage/index.html"

# Build everything for release
release: check-all docker-build helm-package
    @echo "✅ Release artifacts ready!"
    @echo "Docker image: {{IMAGE_NAME}}:{{VERSION}}"
    @ls -lh dist/
