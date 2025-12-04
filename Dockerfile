# Build stage
FROM rust:1.91.1-slim AS builder

WORKDIR /usr/src/app

# Install required dependencies
RUN apt-get update && \
    apt-get install -y pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*

# Copy manifests
COPY Cargo.toml Cargo.lock* ./

# Create dummy source files to build dependencies
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    echo "" > src/lib.rs && \
    cargo build --release && \
    rm -rf src

# Copy the actual source code
COPY src ./src

# Build the application
RUN touch src/main.rs src/lib.rs && \
    cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install CA certificates and OpenSSL for HTTPS
RUN apt-get update && \
    apt-get install -y ca-certificates libssl3 && \
    rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -u 1000 reaper

WORKDIR /app

# Copy the binary from builder
COPY --from=builder /usr/src/app/target/release/pvc-reaper /app/pvc-reaper

# Use non-root user
USER reaper

ENTRYPOINT ["/app/pvc-reaper"]
