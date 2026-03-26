REGISTRY ?= registry.gt.lo:5000
IMAGE    ?= $(REGISTRY)/nextnfs
VERSION  ?= 0.10.0

.PHONY: build build-x86 build-arm64 container-x86 container-arm64 push rpm-x86 rpm-arm64 deb-x86 deb-arm64 clean

# Build for current platform (debug)
build:
	cargo build

# Build static x86_64 binary
build-x86:
	cargo build --release --target x86_64-unknown-linux-musl
	x86_64-linux-musl-strip target/x86_64-unknown-linux-musl/release/nextnfs
	@ls -lh target/x86_64-unknown-linux-musl/release/nextnfs

# Build static aarch64 binary (for MikroTik Rose)
build-arm64:
	cargo build --release --target aarch64-unknown-linux-musl
	aarch64-linux-musl-strip target/aarch64-unknown-linux-musl/release/nextnfs
	@ls -lh target/aarch64-unknown-linux-musl/release/nextnfs

# Build x86_64 container (for Fedora CoreOS)
container-x86: build-x86
	podman build --format docker --tls-verify=false -f Containerfile.x86_64 -t $(IMAGE):$(VERSION) -t $(IMAGE):latest .

# Build aarch64 container (for MikroTik Rose)
container-arm64: build-arm64
	podman build --format docker --tls-verify=false -f Containerfile -t $(IMAGE):$(VERSION) -t $(IMAGE):latest .

# Push container image
push:
	podman push --tls-verify=false $(IMAGE):$(VERSION)
	podman push --tls-verify=false $(IMAGE):latest

# Build RPM package (Fedora/RHEL)
rpm-x86: build-x86
	./packaging/build-rpm.sh x86_64

rpm-arm64: build-arm64
	./packaging/build-rpm.sh aarch64

# Build DEB package (Debian/Ubuntu)
deb-x86: build-x86
	./packaging/build-deb.sh amd64

deb-arm64: build-arm64
	./packaging/build-deb.sh arm64

clean:
	cargo clean
	rm -rf dist/
