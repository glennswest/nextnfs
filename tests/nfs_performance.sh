#!/bin/bash
# nfs_performance.sh — NFS performance benchmarks using fio and dd
#
# Measures: throughput, latency, metadata ops/sec, concurrency scaling
# Outputs: JSON results + human-readable summary table
# Requires: fio, mounted NFS export at $NFS_MOUNT

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

WORK="$NFS_MOUNT/perf_tests"
PERF_JSON="$RESULTS_DIR/nfs_performance.json"

setup() {
    mkdir -p "$WORK"
}

teardown() {
    rm -rf "$WORK" 2>/dev/null || true
}

# ── JSON output accumulator ──────────────────────────────────────────────────

PERF_RESULTS=""

add_perf_result() {
    local name="$1" metric="$2" value="$3" unit="$4"
    local entry="{\"name\":\"$name\",\"metric\":\"$metric\",\"value\":$value,\"unit\":\"$unit\"}"
    if [ -n "$PERF_RESULTS" ]; then
        PERF_RESULTS="$PERF_RESULTS
$entry"
    else
        PERF_RESULTS="$entry"
    fi
}

# ── fio Helpers ──────────────────────────────────────────────────────────────

run_fio() {
    local name="$1" rw="$2" bs="$3" size="$4" numjobs="${5:-1}" extra="${6:-}"
    local fio_out

    fio_out=$(fio --name="$name" \
        --directory="$WORK" \
        --rw="$rw" \
        --bs="$bs" \
        --size="$size" \
        --numjobs="$numjobs" \
        --runtime=30 \
        --time_based=0 \
        --group_reporting \
        --output-format=json \
        --ioengine=posixaio \
        --direct=0 \
        $extra 2>/dev/null) || {
        echo "fio failed for $name"
        return 1
    }

    echo "$fio_out"
}

extract_fio_bw() {
    # Extract bandwidth in KB/s from fio JSON, convert to MB/s
    local json="$1" rw="$2"
    local bw_bytes
    if [ "$rw" = "read" ] || [ "$rw" = "randread" ]; then
        bw_bytes=$(echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['jobs'][0]['read']['bw_bytes'])" 2>/dev/null || echo "0")
    elif [ "$rw" = "write" ] || [ "$rw" = "randwrite" ]; then
        bw_bytes=$(echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['jobs'][0]['write']['bw_bytes'])" 2>/dev/null || echo "0")
    else
        # Mixed — sum read+write
        local rbw wbw
        rbw=$(echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['jobs'][0]['read']['bw_bytes'])" 2>/dev/null || echo "0")
        wbw=$(echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['jobs'][0]['write']['bw_bytes'])" 2>/dev/null || echo "0")
        bw_bytes=$(( rbw + wbw ))
    fi
    echo $(( bw_bytes / 1048576 ))
}

extract_fio_iops() {
    local json="$1" rw="$2"
    if [ "$rw" = "read" ] || [ "$rw" = "randread" ]; then
        echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(int(d['jobs'][0]['read']['iops']))" 2>/dev/null || echo "0"
    elif [ "$rw" = "write" ] || [ "$rw" = "randwrite" ]; then
        echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(int(d['jobs'][0]['write']['iops']))" 2>/dev/null || echo "0"
    else
        local riops wiops
        riops=$(echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(int(d['jobs'][0]['read']['iops']))" 2>/dev/null || echo "0")
        wiops=$(echo "$json" | python3 -c "import sys,json; d=json.load(sys.stdin); print(int(d['jobs'][0]['write']['iops']))" 2>/dev/null || echo "0")
        echo $(( riops + wiops ))
    fi
}

extract_fio_latency() {
    # Returns avg latency in microseconds
    local json="$1" rw="$2"
    local field="read"
    if [ "$rw" = "write" ] || [ "$rw" = "randwrite" ]; then
        field="write"
    fi
    echo "$json" | python3 -c "
import sys, json
d = json.load(sys.stdin)
lat = d['jobs'][0]['${field}']['lat_ns']
print(int(lat['mean'] / 1000))  # ns to us
" 2>/dev/null || echo "0"
}

extract_fio_lat_percentiles() {
    local json="$1" rw="$2"
    local field="read"
    if [ "$rw" = "write" ] || [ "$rw" = "randwrite" ]; then
        field="write"
    fi
    echo "$json" | python3 -c "
import sys, json
d = json.load(sys.stdin)
clat = d['jobs'][0]['${field}']['clat_ns']['percentile']
p50 = int(clat.get('50.000000', 0) / 1000)
p95 = int(clat.get('95.000000', 0) / 1000)
p99 = int(clat.get('99.000000', 0) / 1000)
print(f'{p50} {p95} {p99}')
" 2>/dev/null || echo "0 0 0"
}

# ── Check prerequisites ─────────────────────────────────────────────────────

check_prereqs() {
    if ! has_cmd fio; then
        echo "ERROR: fio not found — install with: dnf install -y fio"
        return 1
    fi
    if ! has_cmd python3; then
        echo "ERROR: python3 not found — needed for JSON parsing"
        return 1
    fi
}

# ── Throughput benchmarks ────────────────────────────────────────────────────

bench_seq_read() {
    echo "  Sequential read (1M blocks, 1G, 1 thread)..."
    # Pre-create the file
    dd if=/dev/urandom of="$WORK/seq_read_file" bs=1M count=1024 2>/dev/null
    local json
    json=$(run_fio "seq_read" "read" "1M" "1G" 1 "--filename=$WORK/seq_read_file") || return 1
    local bw
    bw=$(extract_fio_bw "$json" "read")
    add_perf_result "seq_read" "bandwidth" "$bw" "MB/s"
    printf "    Bandwidth: %s MB/s\n" "$bw"
    rm -f "$WORK/seq_read_file"*
}

bench_seq_write() {
    echo "  Sequential write (1M blocks, 1G, 1 thread)..."
    local json
    json=$(run_fio "seq_write" "write" "1M" "1G" 1) || return 1
    local bw
    bw=$(extract_fio_bw "$json" "write")
    add_perf_result "seq_write" "bandwidth" "$bw" "MB/s"
    printf "    Bandwidth: %s MB/s\n" "$bw"
    rm -f "$WORK/seq_write"*
}

bench_rand_read() {
    echo "  Random read (4K blocks, 256M, 4 threads)..."
    # Pre-create the file
    dd if=/dev/urandom of="$WORK/rand_read_file" bs=1M count=256 2>/dev/null
    local json
    json=$(run_fio "rand_read" "randread" "4K" "256M" 4 "--filename=$WORK/rand_read_file") || return 1
    local iops bw
    iops=$(extract_fio_iops "$json" "randread")
    bw=$(extract_fio_bw "$json" "randread")
    add_perf_result "rand_read" "iops" "$iops" "IOPS"
    add_perf_result "rand_read" "bandwidth" "$bw" "MB/s"
    printf "    IOPS: %s, Bandwidth: %s MB/s\n" "$iops" "$bw"
    rm -f "$WORK/rand_read_file"*
}

bench_rand_write() {
    echo "  Random write (4K blocks, 256M, 4 threads)..."
    local json
    json=$(run_fio "rand_write" "randwrite" "4K" "256M" 4) || return 1
    local iops bw
    iops=$(extract_fio_iops "$json" "randwrite")
    bw=$(extract_fio_bw "$json" "randwrite")
    add_perf_result "rand_write" "iops" "$iops" "IOPS"
    add_perf_result "rand_write" "bandwidth" "$bw" "MB/s"
    printf "    IOPS: %s, Bandwidth: %s MB/s\n" "$iops" "$bw"
    rm -f "$WORK/rand_write"*
}

bench_mixed_rw() {
    echo "  Mixed 70/30 read/write (4K, 4 threads)..."
    local json
    json=$(run_fio "mixed_rw" "randrw" "4K" "256M" 4 "--rwmixread=70") || return 1
    local iops bw
    iops=$(extract_fio_iops "$json" "mixed")
    bw=$(extract_fio_bw "$json" "mixed")
    add_perf_result "mixed_rw" "iops" "$iops" "IOPS"
    add_perf_result "mixed_rw" "bandwidth" "$bw" "MB/s"
    printf "    IOPS: %s, Bandwidth: %s MB/s\n" "$iops" "$bw"
    rm -f "$WORK/mixed_rw"*
}

# ── Latency benchmarks ──────────────────────────────────────────────────────

bench_read_latency() {
    echo "  4K sync read latency..."
    dd if=/dev/urandom of="$WORK/lat_read_file" bs=1M count=64 2>/dev/null
    local json
    json=$(run_fio "lat_read" "randread" "4K" "64M" 1 "--filename=$WORK/lat_read_file --sync=1") || return 1
    local avg_us
    avg_us=$(extract_fio_latency "$json" "read")
    local percentiles
    percentiles=$(extract_fio_lat_percentiles "$json" "read")
    local p50 p95 p99
    p50=$(echo "$percentiles" | awk '{print $1}')
    p95=$(echo "$percentiles" | awk '{print $2}')
    p99=$(echo "$percentiles" | awk '{print $3}')
    add_perf_result "read_latency" "avg_us" "$avg_us" "us"
    add_perf_result "read_latency" "p50_us" "$p50" "us"
    add_perf_result "read_latency" "p95_us" "$p95" "us"
    add_perf_result "read_latency" "p99_us" "$p99" "us"
    printf "    Avg: %s us, P50: %s us, P95: %s us, P99: %s us\n" "$avg_us" "$p50" "$p95" "$p99"
    rm -f "$WORK/lat_read_file"*
}

bench_write_latency() {
    echo "  4K sync write latency..."
    local json
    json=$(run_fio "lat_write" "randwrite" "4K" "64M" 1 "--sync=1 --fsync=1") || return 1
    local avg_us
    avg_us=$(extract_fio_latency "$json" "write")
    local percentiles
    percentiles=$(extract_fio_lat_percentiles "$json" "write")
    local p50 p95 p99
    p50=$(echo "$percentiles" | awk '{print $1}')
    p95=$(echo "$percentiles" | awk '{print $2}')
    p99=$(echo "$percentiles" | awk '{print $3}')
    add_perf_result "write_latency" "avg_us" "$avg_us" "us"
    add_perf_result "write_latency" "p50_us" "$p50" "us"
    add_perf_result "write_latency" "p95_us" "$p95" "us"
    add_perf_result "write_latency" "p99_us" "$p99" "us"
    printf "    Avg: %s us, P50: %s us, P95: %s us, P99: %s us\n" "$avg_us" "$p50" "$p95" "$p99"
    rm -f "$WORK/lat_write"*
}

bench_metadata_latency() {
    echo "  Metadata latency (stat 1000 files)..."
    local dir="$WORK/meta_lat"
    mkdir -p "$dir"
    local i
    for i in $(seq 1 1000); do
        touch "$dir/f$i"
    done

    local start end
    start=$(date +%s%N)
    for i in $(seq 1 1000); do
        stat "$dir/f$i" >/dev/null
    done
    end=$(date +%s%N)

    local total_us=$(( (end - start) / 1000 ))
    local avg_us=$(( total_us / 1000 ))
    add_perf_result "metadata_latency" "stat_avg_us" "$avg_us" "us"
    printf "    stat avg: %s us\n" "$avg_us"
    rm -rf "$dir"
}

# ── Metadata performance ────────────────────────────────────────────────────

bench_metadata_create() {
    echo "  Create 10,000 empty files..."
    local dir="$WORK/meta_create"
    mkdir -p "$dir"

    local start end
    start=$(date +%s%N)
    local i
    for i in $(seq 1 10000); do
        touch "$dir/f$i"
    done
    end=$(date +%s%N)

    local elapsed_ms=$(( (end - start) / 1000000 ))
    local ops_sec=0
    if [ "$elapsed_ms" -gt 0 ]; then
        ops_sec=$(( 10000 * 1000 / elapsed_ms ))
    fi
    add_perf_result "meta_create" "ops_sec" "$ops_sec" "ops/s"
    add_perf_result "meta_create" "elapsed_ms" "$elapsed_ms" "ms"
    printf "    %s ops/s (%s ms)\n" "$ops_sec" "$elapsed_ms"
    # Leave dir for stat and delete tests
}

bench_metadata_stat() {
    echo "  stat 10,000 files..."
    local dir="$WORK/meta_create"
    if [ ! -d "$dir" ]; then
        echo "    SKIP: meta_create dir not found"
        return
    fi

    local start end
    start=$(date +%s%N)
    local i
    for i in $(seq 1 10000); do
        stat "$dir/f$i" >/dev/null
    done
    end=$(date +%s%N)

    local elapsed_ms=$(( (end - start) / 1000000 ))
    local ops_sec=0
    if [ "$elapsed_ms" -gt 0 ]; then
        ops_sec=$(( 10000 * 1000 / elapsed_ms ))
    fi
    add_perf_result "meta_stat" "ops_sec" "$ops_sec" "ops/s"
    add_perf_result "meta_stat" "elapsed_ms" "$elapsed_ms" "ms"
    printf "    %s ops/s (%s ms)\n" "$ops_sec" "$elapsed_ms"
}

bench_metadata_ls() {
    echo "  ls directory with 10,000 entries..."
    local dir="$WORK/meta_create"
    if [ ! -d "$dir" ]; then
        echo "    SKIP: meta_create dir not found"
        return
    fi

    local start end
    start=$(date +%s%N)
    ls "$dir" >/dev/null
    end=$(date +%s%N)

    local elapsed_ms=$(( (end - start) / 1000000 ))
    add_perf_result "meta_ls" "elapsed_ms" "$elapsed_ms" "ms"
    printf "    %s ms\n" "$elapsed_ms"
}

bench_metadata_delete() {
    echo "  Delete 10,000 files..."
    local dir="$WORK/meta_create"
    if [ ! -d "$dir" ]; then
        echo "    SKIP: meta_create dir not found"
        return
    fi

    local start end
    start=$(date +%s%N)
    rm -rf "$dir"
    end=$(date +%s%N)

    local elapsed_ms=$(( (end - start) / 1000000 ))
    local ops_sec=0
    if [ "$elapsed_ms" -gt 0 ]; then
        ops_sec=$(( 10000 * 1000 / elapsed_ms ))
    fi
    add_perf_result "meta_delete" "ops_sec" "$ops_sec" "ops/s"
    add_perf_result "meta_delete" "elapsed_ms" "$elapsed_ms" "ms"
    printf "    %s ops/s (%s ms)\n" "$ops_sec" "$elapsed_ms"
}

# ── Concurrency scaling ─────────────────────────────────────────────────────

bench_concurrency_scaling() {
    echo "  Concurrency scaling (1, 2, 4, 8, 16 threads)..."

    local threads
    for threads in 1 2 4 8 16; do
        local json
        json=$(run_fio "scale_${threads}t" "randread" "4K" "256M" "$threads") || continue
        local iops bw
        iops=$(extract_fio_iops "$json" "randread")
        bw=$(extract_fio_bw "$json" "randread")
        add_perf_result "scaling_${threads}t" "iops" "$iops" "IOPS"
        add_perf_result "scaling_${threads}t" "bandwidth" "$bw" "MB/s"
        printf "    %2d threads: %6s IOPS, %4s MB/s\n" "$threads" "$iops" "$bw"
        rm -f "$WORK/scale_${threads}t"*
    done
}

# ── Save results ─────────────────────────────────────────────────────────────

save_perf_json() {
    local file="$1"
    mkdir -p "$(dirname "$file")"
    {
        echo "{"
        echo "  \"suite\": \"nfs_performance\","
        echo "  \"timestamp\": \"$(date -Iseconds)\","
        echo "  \"results\": ["
        local first=true
        while IFS= read -r line; do
            if [ -n "$line" ]; then
                if $first; then
                    first=false
                else
                    echo ","
                fi
                printf "    %s" "$line"
            fi
        done <<< "$PERF_RESULTS"
        echo ""
        echo "  ]"
        echo "}"
    } > "$file"
}

# ── Summary table ────────────────────────────────────────────────────────────

print_perf_summary() {
    echo ""
    echo "━━━ Performance Summary ━━━"
    echo ""
    printf "%-30s %12s %8s\n" "Benchmark" "Value" "Unit"
    printf "%-30s %12s %8s\n" "──────────────────────────────" "────────────" "────────"

    while IFS= read -r line; do
        if [ -n "$line" ]; then
            local name metric value unit
            name=$(echo "$line" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['name'])" 2>/dev/null)
            metric=$(echo "$line" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['metric'])" 2>/dev/null)
            value=$(echo "$line" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['value'])" 2>/dev/null)
            unit=$(echo "$line" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['unit'])" 2>/dev/null)
            printf "%-30s %12s %8s\n" "${name}/${metric}" "$value" "$unit"
        fi
    done <<< "$PERF_RESULTS"
    echo ""
}

# ── Main ─────────────────────────────────────────────────────────────────────

main() {
    echo -e "${BOLD}━━━ NFS Performance Benchmarks ━━━${RESET}"
    echo ""

    if ! check_prereqs; then
        echo "Prerequisites not met, skipping performance tests"
        exit 0
    fi

    setup

    echo "Throughput:"
    bench_seq_read
    bench_seq_write
    bench_rand_read
    bench_rand_write
    bench_mixed_rw

    echo ""
    echo "Latency:"
    bench_read_latency
    bench_write_latency
    bench_metadata_latency

    echo ""
    echo "Metadata Performance:"
    bench_metadata_create
    bench_metadata_stat
    bench_metadata_ls
    bench_metadata_delete

    echo ""
    bench_concurrency_scaling

    teardown

    save_perf_json "$PERF_JSON"
    print_perf_summary

    echo "Results saved to $PERF_JSON"
}

main "$@"
