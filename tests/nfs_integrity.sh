#!/bin/bash
# nfs_integrity.sh — NFS data integrity validation using Linux kernel source
#
# Downloads a Linux kernel tarball, extracts it, checksums every file,
# then copies the tree N times in parallel to separate directories on the
# NFS mount, and verifies every copy matches the original checksums.
#
# This is the gold standard for NFS correctness testing: it exercises:
#   - Large number of small files (~75,000 files in Linux kernel)
#   - Deep directory trees (20+ levels)
#   - Mixed file sizes (1 byte to several MB)
#   - Parallel concurrent writes (N copies simultaneously)
#   - Complete data integrity verification (SHA-256 of every file)
#
# Usage:
#   NFS_MOUNT=/mnt/nfs-test ./tests/nfs_integrity.sh [--copies 10] [--keep]
#
# Environment:
#   NFS_MOUNT      — mounted NFS export (required)
#   RESULTS_DIR    — output directory (default: /tmp/nextnfs-integrity)
#   KERNEL_URL     — kernel tarball URL (default: latest stable from kernel.org)
#   NUM_COPIES     — number of parallel copies (default: 10)
#   RAMDISK_SIZE   — ramdisk size in MB for baseline test (default: 2048)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# ── Configuration ────────────────────────────────────────────────────────────

NFS_MOUNT="${NFS_MOUNT:-/mnt/nfs-test}"
RESULTS_DIR="${RESULTS_DIR:-/tmp/nextnfs-integrity}"
KERNEL_URL="${KERNEL_URL:-https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.12.tar.xz}"
NUM_COPIES="${NUM_COPIES:-10}"
RAMDISK_SIZE="${RAMDISK_SIZE:-2048}"
KEEP_DATA=0

# Parse arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        --copies) NUM_COPIES="$2"; shift 2 ;;
        --keep) KEEP_DATA=1; shift ;;
        --kernel-url) KERNEL_URL="$2"; shift 2 ;;
        --ramdisk-size) RAMDISK_SIZE="$2"; shift 2 ;;
        --mount) NFS_MOUNT="$2"; shift 2 ;;
        --results) RESULTS_DIR="$2"; shift 2 ;;
        -h|--help)
            echo "Usage: $0 [--copies 10] [--keep] [--kernel-url URL]"
            echo ""
            echo "  --copies N       Number of parallel copies (default: 10)"
            echo "  --keep           Don't clean up after test"
            echo "  --kernel-url     Custom kernel tarball URL"
            echo "  --ramdisk-size   Ramdisk size in MB (default: 2048)"
            echo "  --mount PATH     NFS mount point"
            echo "  --results DIR    Results output directory"
            exit 0
            ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

# ── Colour output ────────────────────────────────────────────────────────────

if [ -t 1 ]; then
    GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[0;33m'
    CYAN='\033[0;36m'; BOLD='\033[1m'; RESET='\033[0m'
else
    GREEN='' RED='' YELLOW='' CYAN='' BOLD='' RESET=''
fi

ok()   { echo -e "  ${GREEN}PASS${RESET}: $1"; }
fail() { echo -e "  ${RED}FAIL${RESET}: $1"; }
info() { echo -e "  ${CYAN}INFO${RESET}: $1"; }
warn() { echo -e "  ${YELLOW}WARN${RESET}: $1"; }

TOTAL_PASS=0
TOTAL_FAIL=0

# ── JSON results ─────────────────────────────────────────────────────────────

JSON_RESULTS=""

add_result() {
    local name="$1" metric="$2" value="$3" unit="$4"
    local entry="{\"name\":\"$name\",\"metric\":\"$metric\",\"value\":$value,\"unit\":\"$unit\"}"
    if [ -n "$JSON_RESULTS" ]; then
        JSON_RESULTS="$JSON_RESULTS
$entry"
    else
        JSON_RESULTS="$entry"
    fi
}

save_json() {
    local file="$1"
    {
        echo "{"
        echo "  \"suite\": \"nfs_integrity\","
        echo "  \"timestamp\": \"$(date -Iseconds)\","
        echo "  \"host\": \"$(hostname)\","
        echo "  \"nfs_mount\": \"$NFS_MOUNT\","
        echo "  \"kernel_url\": \"$KERNEL_URL\","
        echo "  \"num_copies\": $NUM_COPIES,"
        echo "  \"results\": ["
        local first=true
        while IFS= read -r line; do
            if [ -n "$line" ]; then
                if $first; then first=false; else echo ","; fi
                printf "    %s" "$line"
            fi
        done <<< "$JSON_RESULTS"
        echo ""
        echo "  ]"
        echo "}"
    } > "$file"
}

# ── Prerequisites ────────────────────────────────────────────────────────────

check_prereqs() {
    local missing=0

    if [ ! -d "$NFS_MOUNT" ]; then
        echo "ERROR: NFS_MOUNT=$NFS_MOUNT does not exist"
        exit 1
    fi

    for cmd in sha256sum tar curl; do
        if ! command -v "$cmd" >/dev/null 2>&1; then
            # macOS compatibility
            if [ "$cmd" = "sha256sum" ] && command -v shasum >/dev/null 2>&1; then
                continue
            fi
            echo "ERROR: $cmd not found"
            ((missing++))
        fi
    done

    if [ "$missing" -gt 0 ]; then
        exit 1
    fi
}

# SHA-256 wrapper (Linux: sha256sum, macOS: shasum -a 256)
sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$@"
    else
        shasum -a 256 "$@"
    fi
}

# ── Download and extract kernel ──────────────────────────────────────────────

download_kernel() {
    local cache_dir="/tmp/nextnfs-kernel-cache"
    local tarball="$cache_dir/$(basename "$KERNEL_URL")"

    mkdir -p "$cache_dir"

    if [ -f "$tarball" ]; then
        info "Using cached kernel tarball: $tarball"
    else
        info "Downloading kernel: $KERNEL_URL"
        curl -L -o "$tarball" "$KERNEL_URL" || {
            fail "Failed to download kernel tarball"
            exit 1
        }
    fi

    echo "$tarball"
}

extract_kernel() {
    local tarball="$1"
    local dest="$2"

    mkdir -p "$dest"

    info "Extracting kernel to $dest..."
    local start=$(date +%s%N)
    tar xf "$tarball" -C "$dest" --strip-components=1
    local end=$(date +%s%N)
    local elapsed_ms=$(( (end - start) / 1000000 ))

    local file_count
    file_count=$(find "$dest" -type f | wc -l | tr -d ' ')
    local dir_count
    dir_count=$(find "$dest" -type d | wc -l | tr -d ' ')
    local total_size
    total_size=$(du -sm "$dest" 2>/dev/null | awk '{print $1}')

    info "Extracted: $file_count files, $dir_count dirs, ${total_size}MB in ${elapsed_ms}ms"

    add_result "extract" "file_count" "$file_count" "files"
    add_result "extract" "dir_count" "$dir_count" "dirs"
    add_result "extract" "total_size" "${total_size:-0}" "MB"
    add_result "extract" "elapsed" "$elapsed_ms" "ms"

    echo "$file_count"
}

# ── Checksum operations ──────────────────────────────────────────────────────

generate_checksums() {
    local dir="$1"
    local output="$2"

    info "Generating SHA-256 checksums for all files in $dir..."

    local start=$(date +%s%N)

    # Generate checksums relative to the source directory
    (cd "$dir" && find . -type f -print0 | sort -z | xargs -0 sha256 > "$output")

    local end=$(date +%s%N)
    local elapsed_ms=$(( (end - start) / 1000000 ))
    local checksum_count
    checksum_count=$(wc -l < "$output" | tr -d ' ')

    info "Generated $checksum_count checksums in ${elapsed_ms}ms"
    add_result "checksum_gen" "count" "$checksum_count" "files"
    add_result "checksum_gen" "elapsed" "$elapsed_ms" "ms"
}

verify_checksums() {
    local dir="$1"
    local checksums_file="$2"
    local label="$3"

    info "Verifying checksums for $label..."

    local start=$(date +%s%N)

    # Verify checksums from the copy directory
    local failures
    failures=$(cd "$dir" && sha256 -c "$checksums_file" 2>&1 | grep -c "FAILED" || true)

    local end=$(date +%s%N)
    local elapsed_ms=$(( (end - start) / 1000000 ))
    local total
    total=$(wc -l < "$checksums_file" | tr -d ' ')

    if [ "$failures" -eq 0 ]; then
        ok "$label: $total files verified in ${elapsed_ms}ms"
        add_result "verify_$label" "status" "1" "pass"
        ((TOTAL_PASS++))
    else
        fail "$label: $failures/$total files FAILED integrity check"
        add_result "verify_$label" "status" "0" "fail"
        add_result "verify_$label" "failures" "$failures" "files"
        ((TOTAL_FAIL++))
    fi
    add_result "verify_$label" "elapsed" "$elapsed_ms" "ms"
}

# ── Copy operations ─────────────────────────────────────────────────────────

parallel_copy() {
    local source_dir="$1"
    local dest_base="$2"
    local num_copies="$3"

    info "Starting $num_copies parallel copies to NFS..."

    local start=$(date +%s%N)

    local pids=()
    local i
    for i in $(seq 1 "$num_copies"); do
        local dest="$dest_base/copy_$i"
        mkdir -p "$dest"
        (cp -a "$source_dir/." "$dest/") &
        pids+=($!)
    done

    # Wait for all copies
    local failed=0
    for pid in "${pids[@]}"; do
        if ! wait "$pid"; then
            ((failed++))
        fi
    done

    local end=$(date +%s%N)
    local elapsed_ms=$(( (end - start) / 1000000 ))
    local elapsed_sec=$(( elapsed_ms / 1000 ))

    local source_size
    source_size=$(du -sm "$source_dir" 2>/dev/null | awk '{print $1}')
    local total_mbs=$(( source_size * num_copies ))
    local throughput=0
    if [ "$elapsed_sec" -gt 0 ]; then
        throughput=$(( total_mbs / elapsed_sec ))
    fi

    info "$num_copies copies (${total_mbs}MB total) in ${elapsed_sec}s = ${throughput} MB/s"

    add_result "parallel_copy" "copies" "$num_copies" "copies"
    add_result "parallel_copy" "total_data" "$total_mbs" "MB"
    add_result "parallel_copy" "elapsed" "$elapsed_ms" "ms"
    add_result "parallel_copy" "throughput" "$throughput" "MB/s"
    add_result "parallel_copy" "failures" "$failed" "copies"

    if [ "$failed" -gt 0 ]; then
        fail "$failed copy operations failed"
        ((TOTAL_FAIL++))
    else
        ok "All $num_copies copies completed"
        ((TOTAL_PASS++))
    fi
}

# ── Backend-specific tests ──────────────────────────────────────────────────

run_ramdisk_baseline() {
    local ramdisk_mount="/tmp/nextnfs-ramdisk"

    echo ""
    echo -e "${BOLD}Ramdisk Baseline Test${RESET}"
    echo ""

    if [ "$(id -u)" -ne 0 ]; then
        warn "Ramdisk test requires root — skipping"
        return
    fi

    mkdir -p "$ramdisk_mount"

    if mountpoint -q "$ramdisk_mount" 2>/dev/null; then
        umount "$ramdisk_mount"
    fi

    mount -t tmpfs -o size=${RAMDISK_SIZE}m tmpfs "$ramdisk_mount" || {
        warn "Failed to mount ramdisk"
        return
    }
    ok "Ramdisk mounted: ${RAMDISK_SIZE}MB at $ramdisk_mount"

    # Quick fio test on ramdisk
    info "fio sequential write on ramdisk..."
    local json
    json=$(fio --name=ramdisk_write --directory="$ramdisk_mount" \
        --rw=write --bs=1M --size=512M --numjobs=1 --runtime=10 \
        --time_based=0 --group_reporting --output-format=json --ioengine=posixaio --direct=0 2>/dev/null) || true
    local bw=$(echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['jobs'][0]['write']['bw_bytes']//1048576)" 2>/dev/null || echo "0")
    add_result "ramdisk" "seq_write" "$bw" "MB/s"
    printf "    Ramdisk seq write: %s MB/s\n" "$bw"

    info "fio sequential read on ramdisk..."
    dd if=/dev/urandom of="$ramdisk_mount/readtest" bs=1M count=512 2>/dev/null
    json=$(fio --name=ramdisk_read --filename="$ramdisk_mount/readtest" \
        --rw=read --bs=1M --size=512M --numjobs=1 --runtime=10 \
        --time_based=0 --group_reporting --output-format=json --ioengine=posixaio --direct=0 2>/dev/null) || true
    bw=$(echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['jobs'][0]['read']['bw_bytes']//1048576)" 2>/dev/null || echo "0")
    add_result "ramdisk" "seq_read" "$bw" "MB/s"
    printf "    Ramdisk seq read: %s MB/s\n" "$bw"

    info "fio 4K random read IOPS on ramdisk..."
    json=$(fio --name=ramdisk_randread --filename="$ramdisk_mount/readtest" \
        --rw=randread --bs=4k --size=512M --numjobs=4 --runtime=10 \
        --time_based=0 --group_reporting --output-format=json --ioengine=posixaio --direct=0 2>/dev/null) || true
    local iops=$(echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(int(d['jobs'][0]['read']['iops']))" 2>/dev/null || echo "0")
    add_result "ramdisk" "rand_read_4k_iops" "$iops" "IOPS"
    printf "    Ramdisk 4K random read: %s IOPS\n" "$iops"

    umount "$ramdisk_mount" 2>/dev/null || true
    rmdir "$ramdisk_mount" 2>/dev/null || true
}

detect_backend() {
    # Try to detect the storage backend type from mount info
    local mount_dev
    mount_dev=$(df "$NFS_MOUNT" 2>/dev/null | tail -1 | awk '{print $1}')
    local mount_type
    mount_type=$(mount | grep "$NFS_MOUNT" | awk '{print $5}' | head -1)

    info "Mount: $mount_dev ($mount_type)"
    add_result "backend" "mount_device" "0" "$mount_dev"
    add_result "backend" "mount_type" "0" "$mount_type"

    # Check if backing store is rotational (SATA HDD) or SSD
    if [ -n "$mount_dev" ] && [ -b "$mount_dev" ] 2>/dev/null; then
        local base_dev
        base_dev=$(basename "$mount_dev" | sed 's/[0-9]*$//')
        local rotational
        rotational=$(cat "/sys/block/$base_dev/queue/rotational" 2>/dev/null || echo "unknown")
        if [ "$rotational" = "1" ]; then
            info "Backend: SATA/Rotational HDD"
            add_result "backend" "type" "0" "hdd"
        elif [ "$rotational" = "0" ]; then
            info "Backend: SSD/NVMe"
            add_result "backend" "type" "0" "ssd"
        fi
    fi
}

# ── Main ─────────────────────────────────────────────────────────────────────

main() {
    echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
    echo -e "${BOLD}║      nextnfs Data Integrity Validation                      ║${RESET}"
    echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
    echo ""
    echo "  Mount:    $NFS_MOUNT"
    echo "  Copies:   $NUM_COPIES parallel"
    echo "  Kernel:   $(basename "$KERNEL_URL")"
    echo "  Results:  $RESULTS_DIR"
    echo ""

    check_prereqs
    mkdir -p "$RESULTS_DIR"

    detect_backend

    # ── Phase 1: Download kernel ──
    echo ""
    echo -e "${BOLD}Phase 1: Acquire Linux Kernel Source${RESET}"
    echo ""

    local tarball
    tarball=$(download_kernel)

    # ── Phase 2: Extract to local temp ──
    echo ""
    echo -e "${BOLD}Phase 2: Extract Source Tree${RESET}"
    echo ""

    local local_src="/tmp/nextnfs-kernel-src"
    rm -rf "$local_src"
    local file_count
    file_count=$(extract_kernel "$tarball" "$local_src")

    # ── Phase 3: Generate master checksums ──
    echo ""
    echo -e "${BOLD}Phase 3: Generate Master Checksums${RESET}"
    echo ""

    local checksums_file="$RESULTS_DIR/master_checksums.sha256"
    generate_checksums "$local_src" "$checksums_file"

    # ── Phase 4: Copy source to NFS (single copy first) ──
    echo ""
    echo -e "${BOLD}Phase 4: Single Copy to NFS${RESET}"
    echo ""

    local nfs_work="$NFS_MOUNT/integrity_test"
    rm -rf "$nfs_work" 2>/dev/null || true
    mkdir -p "$nfs_work/original"

    local start=$(date +%s%N)
    cp -a "$local_src/." "$nfs_work/original/"
    local end=$(date +%s%N)
    local copy_ms=$(( (end - start) / 1000000 ))
    local src_size
    src_size=$(du -sm "$local_src" 2>/dev/null | awk '{print $1}')
    local copy_mbs=$(( src_size * 1000 / (copy_ms + 1) ))
    add_result "single_copy" "elapsed" "$copy_ms" "ms"
    add_result "single_copy" "throughput" "$copy_mbs" "MB/s"
    info "Single copy: ${src_size}MB in $((copy_ms/1000))s = ${copy_mbs} MB/s"

    # ── Phase 5: Verify NFS copy ──
    echo ""
    echo -e "${BOLD}Phase 5: Verify NFS Copy${RESET}"
    echo ""

    verify_checksums "$nfs_work/original" "$checksums_file" "nfs_original"

    # ── Phase 6: Parallel copies ──
    echo ""
    echo -e "${BOLD}Phase 6: $NUM_COPIES Parallel Copies on NFS${RESET}"
    echo ""

    parallel_copy "$nfs_work/original" "$nfs_work" "$NUM_COPIES"

    # ── Phase 7: Verify all copies ──
    echo ""
    echo -e "${BOLD}Phase 7: Verify All Copies${RESET}"
    echo ""

    local i
    for i in $(seq 1 "$NUM_COPIES"); do
        verify_checksums "$nfs_work/copy_$i" "$checksums_file" "copy_$i"
    done

    # ── Phase 8: Ramdisk baseline ──
    if command -v fio >/dev/null 2>&1; then
        run_ramdisk_baseline
    fi

    # ── Cleanup ──
    if [ "$KEEP_DATA" -eq 0 ]; then
        info "Cleaning up NFS test data..."
        rm -rf "$nfs_work" 2>/dev/null || true
    else
        info "Keeping test data at $nfs_work"
    fi

    # ── Save results ──
    save_json "$RESULTS_DIR/integrity_results.json"

    # ── Summary ──
    echo ""
    echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
    echo -e "${BOLD}║                   Integrity Test Summary                    ║${RESET}"
    echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
    echo ""
    echo "  Files per copy:  $file_count"
    echo "  Total copies:    $NUM_COPIES"
    echo "  Total files:     $(( file_count * (NUM_COPIES + 1) ))"
    echo "  Passed:          $TOTAL_PASS"
    echo "  Failed:          $TOTAL_FAIL"
    echo ""
    echo "  Results:  $RESULTS_DIR/integrity_results.json"
    echo "  Checksums: $checksums_file"
    echo ""

    if [ "$TOTAL_FAIL" -eq 0 ]; then
        echo -e "  ${GREEN}${BOLD}ALL INTEGRITY CHECKS PASSED${RESET}"
        exit 0
    else
        echo -e "  ${RED}${BOLD}$TOTAL_FAIL INTEGRITY CHECKS FAILED${RESET}"
        exit 1
    fi
}

main "$@"
