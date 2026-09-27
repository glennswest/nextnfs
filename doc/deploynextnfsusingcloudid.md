# Deploying NextNFS Using CloudID

> **Status (2026-09-27):** this guide was written for the mkube-managed lab, and mkube was retired on 2026-08-27. The CloudID/Ignition flow and container settings below are still what the `nextnfs.ign.json` template does; the mkube-specific steps (BareMetalHost CRD, `mk annotate`) no longer apply — reboot the host by whatever manages it now.

This guide covers deploying NextNFS on bare metal hosts running Fedora CoreOS (FCOS) using CloudID's template system. CloudID serves an Ignition config at boot time that partitions the disk, sets up the filesystem, and runs the NextNFS container automatically.

## Prerequisites

- A bare metal host that PXE boots Fedora CoreOS (previously managed by mkube's BareMetalHost CRD — mkube is retired)
- CloudID running and reachable at `192.168.200.20:8090`
- The host's network has DNS resolution for `registry.gt.lo` (see Known Issues below)
- The NextNFS container image pushed to the registry

## Step 1: Build and Push the Container Image

The image is built from a locally compiled static musl binary — `podman build .` on its own does not compile anything, and the default `Containerfile` is the **aarch64** variant. Use `build.sh` (or `make container-x86|container-arm64`):

```bash
cd /path/to/nextnfs

# x86_64 (Fedora CoreOS) — cargo build --target x86_64-unknown-linux-musl, then Containerfile.x86_64
./build.sh x86

# or ARM64 — cargo build --target aarch64-unknown-linux-musl, then Containerfile
./build.sh arm64

# Push (build.sh tags both :<version> and :latest)
podman push --tls-verify=false registry.gt.lo:5000/nextnfs:latest
```

The image is `stormdbase` with stormd as PID 1; stormd starts `nextnfs serve --export /export --listen 0.0.0.0:2049 --api-listen 0.0.0.0:8080`. The `/etc/nextnfs/nextnfs.toml` copied into the image is not read unless the process args in `stormd.toml` are changed to `serve --config /etc/nextnfs/nextnfs.toml`.

## Step 2: Upload the Template to CloudID

The template is already committed to the cloudid repo at `templates/fcos/nextnfs.ign.json`. Upload it to the running CloudID instance:

```bash
# Read the template and wrap it for the API
CONTENT=$(cat templates/fcos/nextnfs.ign.json | jq -Rs '.')

curl -s -X PUT http://192.168.200.20:8090/api/v1/templates/fcos/nextnfs.ign.json \
  -H 'Content-Type: application/json' \
  -d "{\"content\": ${CONTENT}, \"mode\": \"forever\"}"
```

The `mode` field controls how the template is served:

| Mode | Behavior |
|------|----------|
| `forever` | Template is served on every boot. Use this for NFS servers that should always come up with the same config. |
| `oneshot` | Template is served on first boot only. After the host calls `POST /config/provisioned`, subsequent boots skip the template. |

For NFS servers, `forever` is recommended so the host always boots into the correct config.

## Step 3: Assign the Template to a Host

Assign the nextnfs template to the target host:

```bash
curl -s -X PUT http://192.168.200.20:8090/api/v1/assignments/server1 \
  -H 'Content-Type: application/json' \
  -d '{"image_type": "fcos", "template": "nextnfs.ign.json"}'
```

Replace `server1` with the hostname of your target BMH.

### Verify the Assignment

```bash
# List all assignments
curl -s http://192.168.200.20:8090/api/v1/assignments | python3 -m json.tool

# Check what template would be served for a specific host (from the host itself)
curl -s http://169.254.169.254/config/template
```

## Step 4: Boot the Host

PXE boot or reboot the host. (Under the retired mkube this was `mk annotate bmh/server1 bmh.mkube.io/reboot=...`; use whatever now manages the host.)

On boot, the following happens automatically:

1. Host gets DHCP lease with metadata route (`169.254.169.254` via gateway)
2. Ignition fetches config from CloudID (via MikroTik DNAT)
3. CloudID resolves the host by source IP, finds the nextnfs template assignment
4. CloudID substitutes variables (`{{HOSTNAME_ENCODED}}`, etc.) and merges SSH keys
5. Ignition applies the config:
   - Partitions `/dev/sda` with a `data` label (preserves existing data if partition exists)
   - Formats as XFS (only if not already formatted)
   - Mounts at `/var/data`
   - Creates `/var/data/nfs` export directory
   - Starts the `nextnfs.service` systemd unit

## Step 5: Verify

After the host boots, SSH in and check the service:

```bash
ssh core@<host-ip>

# Check service status
systemctl status nextnfs.service

# Check the container is running
podman ps

# Check the NFS export directory
ls /var/data/nfs

# Check NFS port is listening
ss -tlnp | grep 2049
```

From another host, test the NFS mount:

```bash
# Mount the NFS export (single export, so / is the export itself)
mount -t nfs4 -o vers=4.1 <nfs-host-ip>:/ /mnt

# Verify
ls /mnt
touch /mnt/testfile
```

## What the Template Does

The ignition template sets up:

### Disk Layout

| Partition | Label | Filesystem | Mount Point |
|-----------|-------|------------|-------------|
| `/dev/sda1` | `data` | XFS | `/var/data` |

The disk is partitioned with `wipeTable: false` and formatted with `wipeFilesystem: false`, meaning:
- If the partition already exists, it is reused
- If the filesystem already exists, it is preserved
- Data survives reboots and re-provisioning

### Systemd Units

| Unit | Type | Purpose |
|------|------|---------|
| `var-data.mount` | mount | Mounts the data partition at `/var/data` |
| `var-data-nfs-setup.service` | oneshot | Creates `/var/data/nfs` directory |
| `nextnfs.service` | simple | Runs the NextNFS container |

### Container Configuration

The NextNFS container runs with:
- `--network host` -- NFS on port 2049, the nextnfs REST API/web UI on 8080 and stormd on 9080, directly on the host network (the API has no authentication)
- `-v /var/data/nfs:/export:z` -- exports the data directory
- `--pull=always` -- pulls the latest image on every restart
- Automatic restart on failure (5 second delay)

### Variable Substitution

CloudID replaces these variables in the template before serving:

| Variable | Example Value |
|----------|---------------|
| `{{HOSTNAME_ENCODED}}` | `server1.g10.lo` |

SSH keys and user accounts are merged automatically by CloudID -- they are not part of the template.

## Removing a Host's Assignment

To stop a host from receiving the nextnfs template:

```bash
curl -s -X DELETE http://192.168.200.20:8090/api/v1/assignments/server1
```

The host will need to be rebooted to pick up the change.

## Troubleshooting

### Service stuck in "Waiting for DNS"

The nextnfs service waits up to 120 seconds for `registry.gt.lo` to resolve. If DNS fails:

```bash
# Check DNS resolution
getent hosts registry.gt.lo

# Check systemd-resolved config
resolvectl status
```

See CloudID's Known Issues section for the cross-network DNS resolution problem and workarounds.

### Container fails to pull

```bash
# Check if registry is reachable
curl -s http://registry.gt.lo:5000/v2/_catalog

# Check insecure registry config exists
cat /etc/containers/registries.conf.d/registry-gt-lo.conf

# Manual pull test
podman pull --tls-verify=false registry.gt.lo:5000/nextnfs:latest
```

### NFS port not listening

```bash
# Check service logs
journalctl -u nextnfs.service -f

# Check container logs
podman logs nextnfs
```

### Data disk not mounted

```bash
# Check mount status
mount | grep /var/data

# Check partition exists
lsblk /dev/sda

# Check filesystem
blkid /dev/disk/by-partlabel/data
```
