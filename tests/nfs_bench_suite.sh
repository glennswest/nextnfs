#!/bin/bash
# nfs_bench_suite.sh — Industry-standard NFS benchmark suite
#
# Runs multiple benchmark tools against an NFS mount and produces
# comparison-ready JSON + markdown reports.
#
# Tools used:
#   - fio          — throughput, latency, IOPS (via nfs_performance.sh)
#   - IOzone       — filesystem throughput, re-read, random, stride
#   - Dbench       — Samba-style concurrent file ops
#   - Bonnie++     — I/O throughput + metadata (create/stat/delete)
#   - SPECstorage-style synthetic workloads (AI, SW Build, Genomics)
#
# Usage:
#   NFS_MOUNT=/mnt/nfs-test ./tests/nfs_bench_suite.sh [--quick] [--tools fio,iozone,dbench]
#
# Environment:
#   NFS_MOUNT    — mounted NFS export (required)
#   RESULTS_DIR  — output directory (default: /tmp/nextnfs-bench)
#   BENCH_SIZE   — data size for large tests (default: 1G)
#   NUM_CLIENTS  — concurrent client simulation count (default: 8)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# ── Configuration ────────────────────────────────────────────────────────────

NFS_MOUNT="${NFS_MOUNT:-/mnt/nfs-test}"
RESULTS_DIR="${RESULTS_DIR:-/tmp/nextnfs-bench}"
BENCH_SIZE="${BENCH_SIZE:-1G}"
NUM_CLIENTS="${NUM_CLIENTS:-8}"
QUICK_MODE=0
TOOLS_LIST="fio,iozone,dbench,bonnie,specsim"

# Parse arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        --quick) QUICK_MODE=1; shift ;;
        --tools) TOOLS_LIST="$2"; shift 2 ;;
        --size) BENCH_SIZE="$2"; shift 2 ;;
        --clients) NUM_CLIENTS="$2"; shift 2 ;;
        --results) RESULTS_DIR="$2"; shift 2 ;;
        --mount) NFS_MOUNT="$2"; shift 2 ;;
        -h|--help)
            echo "Usage: $0 [--quick] [--tools fio,iozone,...] [--size 1G] [--clients 8]"
            echo ""
            echo "Tools: fio, iozone, dbench, bonnie, specsim"
            echo "  --quick     Reduce data sizes and iterations"
            echo "  --tools     Comma-separated list of benchmarks to run"
            echo "  --size      Data size for large tests (default: 1G)"
            echo "  --clients   Concurrent client count (default: 8)"
            echo "  --results   Output directory (default: /tmp/nextnfs-bench)"
            echo "  --mount     NFS mount point (default: /mnt/nfs-test)"
            exit 0
            ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

if [ "$QUICK_MODE" -eq 1 ]; then
    BENCH_SIZE="256M"
    NUM_CLIENTS=4
fi

# ── Colour output ────────────────────────────────────────────────────────────

if [ -t 1 ]; then
    GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[0;33m'
    CYAN='\033[0;36m'; BOLD='\033[1m'; RESET='\033[0m'
else
    GREEN='' RED='' YELLOW='' CYAN='' BOLD='' RESET=''
fi

ok()   { echo -e "  ${GREEN}PASS${RESET}: $1"; }
fail() { echo -e "  ${RED}FAIL${RESET}: $1"; }
skip() { echo -e "  ${YELLOW}SKIP${RESET}: $1"; }
info() { echo -e "  ${CYAN}INFO${RESET}: $1"; }

has_tool() { command -v "$1" >/dev/null 2>&1; }
want_tool() { echo ",$TOOLS_LIST," | grep -q ",$1,"; }

# ── JSON accumulator ────────────────────────────────────────────────────────

JSON_RESULTS=""

add_result() {
    local tool="$1" name="$2" metric="$3" value="$4" unit="$5"
    local entry="{\"tool\":\"$tool\",\"name\":\"$name\",\"metric\":\"$metric\",\"value\":$value,\"unit\":\"$unit\"}"
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
        echo "  \"suite\": \"nfs_bench_suite\","
        echo "  \"timestamp\": \"$(date -Iseconds)\","
        echo "  \"host\": \"$(hostname)\","
        echo "  \"nfs_mount\": \"$NFS_MOUNT\","
        echo "  \"bench_size\": \"$BENCH_SIZE\","
        echo "  \"num_clients\": $NUM_CLIENTS,"
        echo "  \"quick_mode\": $([ "$QUICK_MODE" -eq 1 ] && echo "true" || echo "false"),"
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

check_mount() {
    if [ ! -d "$NFS_MOUNT" ]; then
        echo "ERROR: NFS_MOUNT=$NFS_MOUNT does not exist"
        exit 1
    fi
    # Try creating a file to verify write access
    local testfile="$NFS_MOUNT/.bench_test_$$"
    if touch "$testfile" 2>/dev/null; then
        rm -f "$testfile"
    else
        echo "ERROR: Cannot write to $NFS_MOUNT"
        exit 1
    fi
}

install_tools() {
    info "Checking benchmark tools..."
    local missing=""

    for tool in fio iozone dbench bonnie++; do
        if ! has_tool "$tool"; then
            missing="$missing $tool"
        fi
    done

    if [ -n "$missing" ]; then
        info "Missing:$missing"
        if has_tool dnf; then
            dnf install -y fio iozone dbench bonnie++ 2>/dev/null || true
        elif has_tool apt-get; then
            apt-get update -qq && apt-get install -y fio iozone3 dbench bonnie++ 2>/dev/null || true
        fi
    fi

    # Report what's available
    for tool in fio iozone dbench bonnie++; do
        if has_tool "$tool"; then
            ok "$tool $(${tool} --version 2>&1 | head -1 || echo 'available')"
        else
            skip "$tool not installed"
        fi
    done
}

# ── 1. fio benchmarks ───────────────────────────────────────────────────────

run_fio_bench() {
    if ! want_tool fio || ! has_tool fio; then
        skip "fio"
        return
    fi

    echo ""
    echo -e "${BOLD}[1/5] fio — I/O Performance${RESET}"

    local work="$NFS_MOUNT/bench_fio"
    mkdir -p "$work"

    local size="$BENCH_SIZE"
    local runtime=30
    [ "$QUICK_MODE" -eq 1 ] && runtime=10

    # Sequential read
    info "Sequential read ($size, 1M blocks)..."
    dd if=/dev/urandom of="$work/seqread" bs=1M count=256 2>/dev/null || true
    local json
    json=$(fio --name=seq_read --directory="$work" --filename="$work/seqread" \
        --rw=read --bs=1M --size="$size" --numjobs=1 --runtime=$runtime \
        --time_based=0 --group_reporting --output-format=json --ioengine=posixaio --direct=0 2>/dev/null) || true
    local bw=$(echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['jobs'][0]['read']['bw_bytes']//1048576)" 2>/dev/null || echo "0")
    add_result "fio" "seq_read" "bandwidth" "$bw" "MB/s"
    printf "    Sequential read: %s MB/s\n" "$bw"

    # Sequential write
    info "Sequential write ($size, 1M blocks)..."
    json=$(fio --name=seq_write --directory="$work" \
        --rw=write --bs=1M --size="$size" --numjobs=1 --runtime=$runtime \
        --time_based=0 --group_reporting --output-format=json --ioengine=posixaio --direct=0 2>/dev/null) || true
    bw=$(echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['jobs'][0]['write']['bw_bytes']//1048576)" 2>/dev/null || echo "0")
    add_result "fio" "seq_write" "bandwidth" "$bw" "MB/s"
    printf "    Sequential write: %s MB/s\n" "$bw"

    # Random read 4K
    info "Random read (4K, $NUM_CLIENTS threads)..."
    json=$(fio --name=rand_read --directory="$work" --filename="$work/seqread" \
        --rw=randread --bs=4k --size=256M --numjobs="$NUM_CLIENTS" --runtime=$runtime \
        --time_based=0 --group_reporting --output-format=json --ioengine=posixaio --direct=0 2>/dev/null) || true
    local iops=$(echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(int(d['jobs'][0]['read']['iops']))" 2>/dev/null || echo "0")
    add_result "fio" "rand_read_4k" "iops" "$iops" "IOPS"
    printf "    Random read 4K: %s IOPS\n" "$iops"

    # Random write 4K
    info "Random write (4K, $NUM_CLIENTS threads)..."
    json=$(fio --name=rand_write --directory="$work" \
        --rw=randwrite --bs=4k --size=256M --numjobs="$NUM_CLIENTS" --runtime=$runtime \
        --time_based=0 --group_reporting --output-format=json --ioengine=posixaio --direct=0 2>/dev/null) || true
    iops=$(echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(int(d['jobs'][0]['write']['iops']))" 2>/dev/null || echo "0")
    add_result "fio" "rand_write_4k" "iops" "$iops" "IOPS"
    printf "    Random write 4K: %s IOPS\n" "$iops"

    # Mixed 70/30
    info "Mixed 70/30 read/write..."
    json=$(fio --name=mixed --directory="$work" \
        --rw=randrw --rwmixread=70 --bs=4k --size=256M --numjobs="$NUM_CLIENTS" --runtime=$runtime \
        --time_based=0 --group_reporting --output-format=json --ioengine=posixaio --direct=0 2>/dev/null) || true
    local riops=$(echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(int(d['jobs'][0]['read']['iops']))" 2>/dev/null || echo "0")
    local wiops=$(echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(int(d['jobs'][0]['write']['iops']))" 2>/dev/null || echo "0")
    add_result "fio" "mixed_70_30" "read_iops" "$riops" "IOPS"
    add_result "fio" "mixed_70_30" "write_iops" "$wiops" "IOPS"
    printf "    Mixed 70/30: read %s + write %s IOPS\n" "$riops" "$wiops"

    # 4K sync latency
    info "4K sync read latency..."
    json=$(fio --name=lat --directory="$work" --filename="$work/seqread" \
        --rw=randread --bs=4k --size=64M --numjobs=1 --sync=1 --runtime=$runtime \
        --time_based=0 --group_reporting --output-format=json --ioengine=posixaio --direct=0 2>/dev/null) || true
    local lat_us=$(echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(int(d['jobs'][0]['read']['lat_ns']['mean']/1000))" 2>/dev/null || echo "0")
    local lat_p99=$(echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(int(d['jobs'][0]['read']['clat_ns']['percentile'].get('99.000000',0)/1000))" 2>/dev/null || echo "0")
    add_result "fio" "read_latency" "avg_us" "$lat_us" "us"
    add_result "fio" "read_latency" "p99_us" "$lat_p99" "us"
    printf "    Read latency: avg %s us, p99 %s us\n" "$lat_us" "$lat_p99"

    rm -rf "$work"
    ok "fio benchmarks complete"
}

# ── 2. IOzone benchmarks ────────────────────────────────────────────────────

run_iozone_bench() {
    if ! want_tool iozone || ! has_tool iozone; then
        skip "IOzone"
        return
    fi

    echo ""
    echo -e "${BOLD}[2/5] IOzone — Filesystem Throughput${RESET}"

    local work="$NFS_MOUNT/bench_iozone"
    mkdir -p "$work"

    local fsize=1048576  # 1G in KB
    local recsize=1024   # 1M record
    [ "$QUICK_MODE" -eq 1 ] && fsize=262144  # 256M

    info "Running IOzone auto-test (file=$((fsize/1024))M, rec=${recsize}K)..."

    local output
    output=$(iozone -a -s "${fsize}k" -r "${recsize}k" -f "$work/iozone.dat" \
        -i 0 -i 1 -i 2 -i 8 -R -b "$RESULTS_DIR/iozone.xls" 2>&1) || true

    # Parse key results (KB/s)
    local write_kbs read_kbs reread_kbs rewrite_kbs rand_read_kbs rand_write_kbs

    write_kbs=$(echo "$output" | awk '/^[[:space:]]*'"$fsize"'[[:space:]]*'"$recsize"'/ {print $3}' | head -1)
    read_kbs=$(echo "$output" | awk '/^[[:space:]]*'"$fsize"'[[:space:]]*'"$recsize"'/ {print $5}' | head -1)
    reread_kbs=$(echo "$output" | awk '/^[[:space:]]*'"$fsize"'[[:space:]]*'"$recsize"'/ {print $6}' | head -1)
    rewrite_kbs=$(echo "$output" | awk '/^[[:space:]]*'"$fsize"'[[:space:]]*'"$recsize"'/ {print $4}' | head -1)
    rand_read_kbs=$(echo "$output" | awk '/^[[:space:]]*'"$fsize"'[[:space:]]*'"$recsize"'/ {print $7}' | head -1)
    rand_write_kbs=$(echo "$output" | awk '/^[[:space:]]*'"$fsize"'[[:space:]]*'"$recsize"'/ {print $8}' | head -1)

    for metric in write:write_kbs read:read_kbs reread:reread_kbs rewrite:rewrite_kbs rand_read:rand_read_kbs rand_write:rand_write_kbs; do
        local name="${metric%%:*}"
        local var="${metric##*:}"
        local val="${!var:-0}"
        if [ -n "$val" ] && [ "$val" != "0" ]; then
            local mbs=$(( val / 1024 ))
            add_result "iozone" "$name" "bandwidth" "$mbs" "MB/s"
            printf "    %-15s %s MB/s\n" "$name:" "$mbs"
        fi
    done

    # Save raw output
    echo "$output" > "$RESULTS_DIR/iozone_raw.txt"

    rm -rf "$work"
    ok "IOzone benchmarks complete"
}

# ── 3. Dbench benchmarks ────────────────────────────────────────────────────

run_dbench_bench() {
    if ! want_tool dbench || ! has_tool dbench; then
        skip "Dbench"
        return
    fi

    echo ""
    echo -e "${BOLD}[3/5] Dbench — Concurrent File Operations${RESET}"

    local work="$NFS_MOUNT/bench_dbench"
    mkdir -p "$work"

    local duration=60
    [ "$QUICK_MODE" -eq 1 ] && duration=30

    # Find the loadfile
    local loadfile=""
    for f in /usr/share/dbench/client.txt /usr/local/share/dbench/client.txt /etc/dbench/client.txt; do
        if [ -f "$f" ]; then
            loadfile="$f"
            break
        fi
    done
    if [ -z "$loadfile" ]; then
        skip "Dbench loadfile not found"
        rm -rf "$work"
        return
    fi

    for clients in 1 $NUM_CLIENTS; do
        info "Dbench with $clients clients ($duration sec)..."

        local output
        output=$(dbench "$clients" -D "$work" -t "$duration" -c "$loadfile" 2>&1) || true

        # Parse throughput from final line: "Throughput NNN.NN MB/sec N clients ..."
        local throughput
        throughput=$(echo "$output" | grep -oP 'Throughput\s+\K[0-9.]+' | tail -1)

        if [ -n "$throughput" ]; then
            local tp_int=${throughput%.*}
            add_result "dbench" "${clients}_clients" "throughput" "${tp_int:-0}" "MB/s"
            printf "    %d clients: %s MB/s\n" "$clients" "$throughput"
        fi

        rm -rf "$work"/*
    done

    rm -rf "$work"
    ok "Dbench benchmarks complete"
}

# ── 4. Bonnie++ benchmarks ──────────────────────────────────────────────────

run_bonnie_bench() {
    if ! want_tool bonnie || ! has_tool bonnie++; then
        skip "Bonnie++"
        return
    fi

    echo ""
    echo -e "${BOLD}[4/5] Bonnie++ — I/O and Metadata${RESET}"

    local work="$NFS_MOUNT/bench_bonnie"
    mkdir -p "$work"

    local ram_mb=256  # Bonnie needs size > 2x RAM to bypass cache
    local size_mb=512
    [ "$QUICK_MODE" -eq 1 ] && size_mb=256

    local num_files=1024
    [ "$QUICK_MODE" -eq 1 ] && num_files=256

    info "Running Bonnie++ (size=${size_mb}M, files=${num_files})..."

    local output
    output=$(bonnie++ -d "$work" -s "$size_mb" -r "$ram_mb" -n "$num_files" \
        -u root -q 2>&1) || true

    # Save raw output
    echo "$output" > "$RESULTS_DIR/bonnie_raw.txt"

    # Bonnie++ CSV format: fields are comma-separated
    # Parse the single-line CSV result
    local csv_line
    csv_line=$(echo "$output" | tail -1)

    if [ -n "$csv_line" ]; then
        # Fields: name,size,seq_char_put_KBs,seq_char_put_pct,...
        # Field 3: seq output char (KB/s)
        # Field 5: seq output block (KB/s)
        # Field 7: seq output rewrite (KB/s)
        # Field 9: seq input char (KB/s)
        # Field 11: seq input block (KB/s)
        # Field 13: random seeks/s
        local seq_write seq_rewrite seq_read random_seeks
        seq_write=$(echo "$csv_line" | cut -d',' -f5)
        seq_rewrite=$(echo "$csv_line" | cut -d',' -f7)
        seq_read=$(echo "$csv_line" | cut -d',' -f11)
        random_seeks=$(echo "$csv_line" | cut -d',' -f13)

        for metric in "seq_write:$seq_write" "seq_rewrite:$seq_rewrite" "seq_read:$seq_read"; do
            local name="${metric%%:*}"
            local val="${metric##*:}"
            if [ -n "$val" ] && [ "$val" != "+++" ] && [ "$val" != "0" ]; then
                local mbs=$(( val / 1024 ))
                add_result "bonnie" "$name" "bandwidth" "${mbs:-0}" "MB/s"
                printf "    %-15s %s MB/s\n" "$name:" "$mbs"
            fi
        done

        if [ -n "$random_seeks" ] && [ "$random_seeks" != "+++" ]; then
            add_result "bonnie" "random_seeks" "ops" "${random_seeks:-0}" "seeks/s"
            printf "    random_seeks:   %s seeks/s\n" "$random_seeks"
        fi

        # Metadata fields (file creation/stat/deletion per second)
        # Field 15: seq create/s, 17: seq stat/s, 19: seq delete/s
        local seq_create seq_stat seq_delete
        seq_create=$(echo "$csv_line" | cut -d',' -f15)
        seq_stat=$(echo "$csv_line" | cut -d',' -f17)
        seq_delete=$(echo "$csv_line" | cut -d',' -f19)

        for metric in "meta_create:$seq_create" "meta_stat:$seq_stat" "meta_delete:$seq_delete"; do
            local name="${metric%%:*}"
            local val="${metric##*:}"
            if [ -n "$val" ] && [ "$val" != "+++" ] && [ "$val" != "0" ]; then
                add_result "bonnie" "$name" "ops" "${val:-0}" "ops/s"
                printf "    %-15s %s ops/s\n" "$name:" "$val"
            fi
        done
    fi

    rm -rf "$work"
    ok "Bonnie++ benchmarks complete"
}

# ── 5. SPECstorage-style Workload Simulations ───────────────────────────────

run_specsim_bench() {
    if ! want_tool specsim; then
        skip "SPECstorage simulations"
        return
    fi

    echo ""
    echo -e "${BOLD}[5/5] SPECstorage-Style Workload Simulations${RESET}"

    local work="$NFS_MOUNT/bench_specsim"
    mkdir -p "$work"

    # ── AI/Image Processing workload ──
    # Simulates TensorFlow training: many small reads (dataset), large checkpoint writes
    # Pattern: create many small files (images), sequential read all, write large checkpoints
    specsim_ai "$work/ai"

    # ── Software Build workload ──
    # Simulates parallel make: many stat/open/read/close cycles, compile outputs
    # Pattern: create source tree, stat-heavy traversal, parallel small writes
    specsim_swbuild "$work/swbuild"

    # ── Genomics workload ──
    # Models genetic analysis: large sequential reads, moderate writes
    # Pattern: create large input files, read sequentially, write output
    specsim_genomics "$work/genomics"

    rm -rf "$work"
    ok "SPECstorage simulations complete"
}

specsim_ai() {
    local work="$1"
    mkdir -p "$work/dataset" "$work/checkpoints"

    local num_images=2000
    local img_size=50000  # ~50KB per image
    [ "$QUICK_MODE" -eq 1 ] && num_images=500

    info "AI/Image Processing: $num_images images, checkpoint writes..."

    # Phase 1: Dataset creation (simulating dataset download)
    local start=$(date +%s%N)
    local i
    for i in $(seq 1 $num_images); do
        dd if=/dev/urandom of="$work/dataset/img_$(printf '%05d' $i).bin" bs=$img_size count=1 2>/dev/null
    done
    local create_end=$(date +%s%N)
    local create_ms=$(( (create_end - start) / 1000000 ))
    local create_ops=$(( num_images * 1000 / (create_ms + 1) ))
    add_result "specsim_ai" "dataset_ingest" "ops" "$create_ops" "files/s"
    printf "    Dataset ingest: %s files/s (%d files in %d ms)\n" "$create_ops" "$num_images" "$create_ms"

    # Phase 2: Epoch read (sequential scan of all images)
    start=$(date +%s%N)
    for i in $(seq 1 $num_images); do
        cat "$work/dataset/img_$(printf '%05d' $i).bin" > /dev/null
    done
    local read_end=$(date +%s%N)
    local read_ms=$(( (read_end - start) / 1000000 ))
    local read_mbs=$(( num_images * img_size / (read_ms + 1) / 1024 ))
    add_result "specsim_ai" "epoch_read" "bandwidth" "$read_mbs" "MB/s"
    printf "    Epoch read: %s MB/s (%d ms)\n" "$read_mbs" "$read_ms"

    # Phase 3: Checkpoint write (1 large model save)
    local ckpt_size=104857600  # 100MB
    [ "$QUICK_MODE" -eq 1 ] && ckpt_size=26214400  # 25MB
    start=$(date +%s%N)
    dd if=/dev/urandom of="$work/checkpoints/model_epoch_1.ckpt" bs=1M count=$((ckpt_size / 1048576)) 2>/dev/null
    local ckpt_end=$(date +%s%N)
    local ckpt_ms=$(( (ckpt_end - start) / 1000000 ))
    local ckpt_mbs=$(( ckpt_size / (ckpt_ms + 1) / 1024 ))
    add_result "specsim_ai" "checkpoint_write" "bandwidth" "$ckpt_mbs" "MB/s"
    printf "    Checkpoint write: %s MB/s (%d ms)\n" "$ckpt_mbs" "$ckpt_ms"

    rm -rf "$work"
}

specsim_swbuild() {
    local work="$1"
    mkdir -p "$work"

    local num_files=1000
    local src_size=4096   # 4KB source files
    [ "$QUICK_MODE" -eq 1 ] && num_files=250

    info "Software Build: $num_files source files, parallel compile simulation..."

    # Phase 1: Create source tree (simulate checkout)
    local start=$(date +%s%N)
    local depth dirs=("$work")
    for depth in 1 2 3 4 5; do
        local new_dirs=()
        for d in "${dirs[@]}"; do
            mkdir -p "$d/src_$depth" "$d/inc_$depth" 2>/dev/null || true
            new_dirs+=("$d/src_$depth" "$d/inc_$depth")
        done
        dirs=("${new_dirs[@]}")
    done

    local i
    for i in $(seq 1 $num_files); do
        local dir_idx=$(( i % ${#dirs[@]} ))
        dd if=/dev/urandom of="${dirs[$dir_idx]}/file_$i.c" bs=$src_size count=1 2>/dev/null
    done
    local create_end=$(date +%s%N)
    local create_ms=$(( (create_end - start) / 1000000 ))
    add_result "specsim_swbuild" "checkout" "elapsed_ms" "$create_ms" "ms"
    printf "    Source checkout: %d files in %d ms\n" "$num_files" "$create_ms"

    # Phase 2: Stat-heavy traversal (simulating dependency scanning / make -j)
    start=$(date +%s%N)
    find "$work" -name "*.c" -exec stat {} \; > /dev/null 2>&1
    local stat_end=$(date +%s%N)
    local stat_ms=$(( (stat_end - start) / 1000000 ))
    local stat_ops=$(( num_files * 1000 / (stat_ms + 1) ))
    add_result "specsim_swbuild" "dep_scan" "ops" "$stat_ops" "stats/s"
    printf "    Dep scan: %s stats/s (%d ms)\n" "$stat_ops" "$stat_ms"

    # Phase 3: Parallel compile — read source + write object files
    start=$(date +%s%N)
    local obj_count=0
    for f in $(find "$work" -name "*.c" | head -$num_files); do
        cat "$f" > /dev/null
        dd if=/dev/urandom of="${f%.c}.o" bs=$((src_size * 2)) count=1 2>/dev/null
        ((obj_count++))
    done
    local compile_end=$(date +%s%N)
    local compile_ms=$(( (compile_end - start) / 1000000 ))
    local compile_ops=$(( obj_count * 1000 / (compile_ms + 1) ))
    add_result "specsim_swbuild" "compile" "ops" "$compile_ops" "files/s"
    printf "    Compile: %s files/s (%d files in %d ms)\n" "$compile_ops" "$obj_count" "$compile_ms"

    # Phase 4: Link — read all .o, write one large binary
    start=$(date +%s%N)
    find "$work" -name "*.o" -exec cat {} \; > "$work/output.bin" 2>/dev/null
    local link_end=$(date +%s%N)
    local link_ms=$(( (link_end - start) / 1000000 ))
    local link_size=$(stat -c%s "$work/output.bin" 2>/dev/null || stat -f%z "$work/output.bin" 2>/dev/null || echo 0)
    local link_mbs=$(( link_size / (link_ms + 1) / 1024 ))
    add_result "specsim_swbuild" "link" "bandwidth" "$link_mbs" "MB/s"
    printf "    Link: %s MB/s (%d ms)\n" "$link_mbs" "$link_ms"

    rm -rf "$work"
}

specsim_genomics() {
    local work="$1"
    mkdir -p "$work/input" "$work/output"

    local input_size_mb=512
    [ "$QUICK_MODE" -eq 1 ] && input_size_mb=128

    info "Genomics: ${input_size_mb}MB input, sequential read+write..."

    # Phase 1: Create genome data file
    local start=$(date +%s%N)
    dd if=/dev/urandom of="$work/input/genome.fastq" bs=1M count=$input_size_mb 2>/dev/null
    local create_end=$(date +%s%N)
    local create_ms=$(( (create_end - start) / 1000000 ))
    local create_mbs=$(( input_size_mb * 1000 / (create_ms + 1) ))
    add_result "specsim_genomics" "write_input" "bandwidth" "$create_mbs" "MB/s"
    printf "    Write input: %s MB/s (%d MB in %d ms)\n" "$create_mbs" "$input_size_mb" "$create_ms"

    # Phase 2: Sequential alignment read (read full file)
    start=$(date +%s%N)
    cat "$work/input/genome.fastq" > /dev/null
    local read_end=$(date +%s%N)
    local read_ms=$(( (read_end - start) / 1000000 ))
    local read_mbs=$(( input_size_mb * 1000 / (read_ms + 1) ))
    add_result "specsim_genomics" "seq_read" "bandwidth" "$read_mbs" "MB/s"
    printf "    Sequential read: %s MB/s (%d ms)\n" "$read_mbs" "$read_ms"

    # Phase 3: Alignment output (write ~30% of input as BAM)
    local output_mb=$(( input_size_mb * 30 / 100 ))
    start=$(date +%s%N)
    dd if=/dev/urandom of="$work/output/aligned.bam" bs=1M count=$output_mb 2>/dev/null
    local write_end=$(date +%s%N)
    local write_ms=$(( (write_end - start) / 1000000 ))
    local write_mbs=$(( output_mb * 1000 / (write_ms + 1) ))
    add_result "specsim_genomics" "write_output" "bandwidth" "$write_mbs" "MB/s"
    printf "    Write output: %s MB/s (%d MB in %d ms)\n" "$write_mbs" "$output_mb" "$write_ms"

    # Phase 4: Index creation (many small random reads + small writes)
    start=$(date +%s%N)
    local idx_count=100
    for i in $(seq 1 $idx_count); do
        dd if="$work/input/genome.fastq" of=/dev/null bs=4096 count=1 skip=$((RANDOM % (input_size_mb * 256))) 2>/dev/null || true
        dd if=/dev/urandom of="$work/output/idx_$i.bin" bs=1024 count=1 2>/dev/null
    done
    local idx_end=$(date +%s%N)
    local idx_ms=$(( (idx_end - start) / 1000000 ))
    local idx_ops=$(( idx_count * 1000 / (idx_ms + 1) ))
    add_result "specsim_genomics" "indexing" "ops" "$idx_ops" "ops/s"
    printf "    Indexing: %s ops/s (%d ms)\n" "$idx_ops" "$idx_ms"

    rm -rf "$work"
}

# ── Report ──────────────────────────────────────────────────────────────────

generate_report() {
    local report="$RESULTS_DIR/bench_report.md"

    {
        echo "# nextnfs Benchmark Report"
        echo ""
        echo "- Date: $(date)"
        echo "- Host: $(hostname)"
        echo "- NFS Mount: $NFS_MOUNT"
        echo "- Bench Size: $BENCH_SIZE"
        echo "- Clients: $NUM_CLIENTS"
        echo "- Quick Mode: $([ "$QUICK_MODE" -eq 1 ] && echo 'Yes' || echo 'No')"
        echo ""

        local current_tool=""
        while IFS= read -r line; do
            if [ -n "$line" ]; then
                local tool name metric value unit
                tool=$(echo "$line" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['tool'])" 2>/dev/null)
                name=$(echo "$line" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['name'])" 2>/dev/null)
                metric=$(echo "$line" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['metric'])" 2>/dev/null)
                value=$(echo "$line" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['value'])" 2>/dev/null)
                unit=$(echo "$line" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['unit'])" 2>/dev/null)

                if [ "$tool" != "$current_tool" ]; then
                    current_tool="$tool"
                    echo ""
                    echo "## $tool"
                    echo ""
                    echo "| Benchmark | Metric | Value | Unit |"
                    echo "|-----------|--------|-------|------|"
                fi
                echo "| $name | $metric | $value | $unit |"
            fi
        done <<< "$JSON_RESULTS"
        echo ""
    } > "$report"

    info "Report: $report"
}

# ── Main ─────────────────────────────────────────────────────────────────────

main() {
    echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
    echo -e "${BOLD}║       nextnfs Industry Benchmark Suite                      ║${RESET}"
    echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
    echo ""
    echo "  Mount:    $NFS_MOUNT"
    echo "  Size:     $BENCH_SIZE"
    echo "  Clients:  $NUM_CLIENTS"
    echo "  Quick:    $([ "$QUICK_MODE" -eq 1 ] && echo 'Yes' || echo 'No')"
    echo "  Tools:    $TOOLS_LIST"
    echo "  Results:  $RESULTS_DIR"
    echo ""

    check_mount
    mkdir -p "$RESULTS_DIR"
    install_tools

    run_fio_bench
    run_iozone_bench
    run_dbench_bench
    run_bonnie_bench
    run_specsim_bench

    save_json "$RESULTS_DIR/bench_suite.json"
    generate_report

    echo ""
    echo -e "${BOLD}━━━ Benchmark Suite Complete ━━━${RESET}"
    echo ""
    echo "  JSON:   $RESULTS_DIR/bench_suite.json"
    echo "  Report: $RESULTS_DIR/bench_report.md"
    echo ""

    # Print quick summary
    echo -e "${BOLD}Quick Summary:${RESET}"
    while IFS= read -r line; do
        if [ -n "$line" ]; then
            local tool name metric value unit
            tool=$(echo "$line" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['tool'])" 2>/dev/null)
            name=$(echo "$line" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['name'])" 2>/dev/null)
            value=$(echo "$line" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['value'])" 2>/dev/null)
            unit=$(echo "$line" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['unit'])" 2>/dev/null)
            printf "  %-12s %-20s %10s %s\n" "[$tool]" "$name" "$value" "$unit"
        fi
    done <<< "$JSON_RESULTS"
}

main "$@"
