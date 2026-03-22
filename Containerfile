FROM docker.io/library/rust:1.85-bookworm AS builder

ARG TARGETARCH

RUN apt-get update && apt-get install -y musl-tools && rm -rf /var/lib/apt/lists/*

# Install musl targets
RUN rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

# Install cross-compilation linker for aarch64
RUN if [ "$TARGETARCH" = "arm64" ]; then \
        apt-get update && apt-get install -y gcc-aarch64-linux-gnu && rm -rf /var/lib/apt/lists/*; \
    fi

WORKDIR /build

# Copy manifests first for dependency caching
COPY Cargo.toml Cargo.lock ./
COPY proto/Cargo.toml proto/
COPY nfs/Cargo.toml nfs/

# Create stub files so cargo can resolve the workspace
RUN mkdir -p src proto/src nfs/src/server && \
    echo 'fn main() {}' > src/main.rs && \
    echo '' > proto/src/lib.rs && \
    echo '' > nfs/src/lib.rs

# Pre-build dependencies
RUN if [ "$TARGETARCH" = "arm64" ]; then \
        export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc && \
        cargo build --release --target aarch64-unknown-linux-musl || true; \
    else \
        cargo build --release --target x86_64-unknown-linux-musl || true; \
    fi

# Copy full source
COPY . .

# Touch source files to invalidate stub cache
RUN touch src/main.rs proto/src/lib.rs nfs/src/lib.rs

# Build the real binary
RUN if [ "$TARGETARCH" = "arm64" ]; then \
        export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc && \
        cargo build --release --target aarch64-unknown-linux-musl && \
        cp target/aarch64-unknown-linux-musl/release/nextnfs /nextnfs; \
    else \
        cargo build --release --target x86_64-unknown-linux-musl && \
        cp target/x86_64-unknown-linux-musl/release/nextnfs /nextnfs; \
    fi

RUN strip /nextnfs

FROM scratch

COPY --from=builder /nextnfs /nextnfs

EXPOSE 2049

ENTRYPOINT ["/nextnfs"]
CMD ["--export", "/export", "--listen", "0.0.0.0:2049"]
