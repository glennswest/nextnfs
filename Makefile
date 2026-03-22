REGISTRY ?= ghcr.io/glennswest
IMAGE    ?= $(REGISTRY)/nextnfs
VERSION  ?= 0.1.0

.PHONY: build build-x86 build-arm64 container push clean

# Build for current platform (debug)
build:
	cargo build

# Build static x86_64 binary
build-x86:
	cargo build --release --target x86_64-unknown-linux-musl
	strip target/x86_64-unknown-linux-musl/release/nextnfs
	@ls -lh target/x86_64-unknown-linux-musl/release/nextnfs

# Build static aarch64 binary (for MikroTik Rose)
build-arm64:
	cargo build --release --target aarch64-unknown-linux-musl
	strip target/aarch64-unknown-linux-musl/release/nextnfs
	@ls -lh target/aarch64-unknown-linux-musl/release/nextnfs

# Build container image (current platform)
container:
	podman build -t $(IMAGE):$(VERSION) .
	podman tag $(IMAGE):$(VERSION) $(IMAGE):latest

# Push container image
push:
	podman push $(IMAGE):$(VERSION)
	podman push $(IMAGE):latest

clean:
	cargo clean
