#!/bin/bash
# nfs4_stress.sh — NFSv4.0 stress and concurrency tests
#
# Tests: mass file creation, parallel ops, deep paths, mount cycling
# Requires: mounted NFS export at $NFS_MOUNT

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

WORK="$NFS_MOUNT/stress_tests"

setup() {
    mkdir -p "$WORK"
}

teardown() {
    rm -rf "$WORK" 2>/dev/null || true
}

# ── Mass File Operations ─────────────────────────────────────────────────────

test_create_10k_files() {
    local dir="$WORK/tenk_files"
    mkdir -p "$dir"

    local i
    for i in $(seq 1 10000); do
        echo "$i" > "$dir/file_$i"
    done

    local count
    count=$(ls "$dir" | wc -l)
    assert_eq "$(echo "$count" | tr -d ' ')" "10000" "created 10000 files"
}

test_list_10k_directory() {
    local dir="$WORK/tenk_files"
    # Depends on previous test having created the files
    if [ ! -d "$dir" ]; then
        echo "SKIP: 10K files not created"
        return 77
    fi

    local count
    count=$(ls "$dir" | wc -l)
    assert_eq "$(echo "$count" | tr -d ' ')" "10000" "readdir lists 10000 entries"
}

test_delete_10k_files() {
    local dir="$WORK/tenk_files"
    if [ ! -d "$dir" ]; then
        echo "SKIP: 10K files not created"
        return 77
    fi

    rm -rf "$dir"
    assert_file_not_exists "$dir" "deleted 10000 files"
}

test_create_1k_dirs_with_files() {
    local base="$WORK/dirs_with_files"
    mkdir -p "$base"

    local d
    for d in $(seq 1 1000); do
        mkdir -p "$base/dir_$d"
        local f
        for f in $(seq 1 10); do
            echo "d${d}f${f}" > "$base/dir_$d/file_$f"
        done
    done

    # Verify: should have 1000 dirs
    local dcount
    dcount=$(ls "$base" | wc -l)
    assert_eq "$(echo "$dcount" | tr -d ' ')" "1000" "1000 directories"

    # Spot check a few
    assert_file_content "$base/dir_1/file_1" "d1f1" "dir_1/file_1 content"
    assert_file_content "$base/dir_500/file_5" "d500f5" "dir_500/file_5 content"
    assert_file_content "$base/dir_1000/file_10" "d1000f10" "dir_1000/file_10 content"

    rm -rf "$base"
}

# ── Parallel Operations ──────────────────────────────────────────────────────

test_parallel_50_workers() {
    local base="$WORK/parallel"
    mkdir -p "$base"

    local pids=()
    local worker
    for worker in $(seq 1 50); do
        (
            local wdir="$base/worker_$worker"
            mkdir -p "$wdir"
            local j
            for j in $(seq 1 20); do
                echo "w${worker}_f${j}" > "$wdir/file_$j"
            done
            # Read back
            for j in $(seq 1 20); do
                cat "$wdir/file_$j" >/dev/null
            done
            # Delete
            rm -rf "$wdir"
        ) &
        pids+=($!)
    done

    # Wait for all workers
    local failures=0
    local pid
    for pid in "${pids[@]}"; do
        wait "$pid" || ((failures++))
    done

    assert_eq "$failures" "0" "all 50 workers completed without error"
}

test_parallel_create_same_dir() {
    local dir="$WORK/parallel_same"
    mkdir -p "$dir"

    local pids=()
    local worker
    for worker in $(seq 1 20); do
        (
            local j
            for j in $(seq 1 50); do
                echo "w${worker}" > "$dir/w${worker}_f${j}" 2>/dev/null || true
            done
        ) &
        pids+=($!)
    done

    local pid
    for pid in "${pids[@]}"; do
        wait "$pid" || true
    done

    # Should have 1000 files total (20 workers * 50 files)
    local count
    count=$(ls "$dir" | wc -l)
    assert_eq "$(echo "$count" | tr -d ' ')" "1000" "20x50 parallel creates"
    rm -rf "$dir"
}

# ── Deep Path Traversal ─────────────────────────────────────────────────────

test_deep_path_100_levels() {
    local path="$WORK/deep100"
    local i
    for i in $(seq 1 100); do
        path="$path/level_$i"
    done

    mkdir -p "$path"
    assert_is_dir "$path" "100-level deep directory"

    echo "bottom" > "$path/leaf"
    assert_file_content "$path/leaf" "bottom" "file at depth 100"

    rm -rf "$WORK/deep100"
}

# ── Mount/Unmount Cycling ────────────────────────────────────────────────────

test_mount_unmount_cycles() {
    # Write a sentinel file
    echo "sentinel" > "$WORK/sentinel"

    local i
    for i in $(seq 1 10); do
        nfs_unmount "$NFS_MOUNT"
        sleep 0.5
        nfs_mount "$NFS_VERS" "$NFS_EXPORT" "$NFS_MOUNT"
        sleep 0.5

        # Verify data survives
        if [ ! -f "$WORK/sentinel" ]; then
            echo "FAIL: sentinel gone after cycle $i"
            return 1
        fi
        local content
        content=$(cat "$WORK/sentinel")
        if [ "$content" != "sentinel" ]; then
            echo "FAIL: sentinel content wrong after cycle $i: $content"
            return 1
        fi
    done
    rm -f "$WORK/sentinel"
}

# ── Mixed Workload ───────────────────────────────────────────────────────────

test_mixed_workload() {
    # Simulates realistic usage: creating, reading, writing, deleting concurrently
    local base="$WORK/mixed"
    mkdir -p "$base"

    # Writer: creates files
    (
        local i
        for i in $(seq 1 200); do
            echo "data_$i" > "$base/file_$i"
        done
    ) &
    local writer=$!

    # Reader: reads whatever exists
    (
        sleep 0.1
        local i
        for i in $(seq 1 200); do
            cat "$base/file_$i" >/dev/null 2>/dev/null || true
        done
    ) &
    local reader=$!

    # Deleter: removes old files (with delay)
    (
        sleep 0.5
        local i
        for i in $(seq 1 100); do
            rm -f "$base/file_$i" 2>/dev/null || true
        done
    ) &
    local deleter=$!

    wait "$writer" "$reader" "$deleter"

    # Should have ~100 files remaining (101-200)
    local count
    count=$(ls "$base" 2>/dev/null | wc -l)
    assert_gt "$(echo "$count" | tr -d ' ')" "50" "mixed workload: files remain"
    rm -rf "$base"
}

# ── Run all tests ────────────────────────────────────────────────────────────

main() {
    suite_start "NFSv4.0 Stress & Concurrency"

    setup

    # Mass File Operations
    run_test "create 10,000 files"                test_create_10k_files
    run_test "list 10,000 entry directory"         test_list_10k_directory
    run_test "delete 10,000 files"                test_delete_10k_files
    run_test "1,000 dirs with 10 files each"      test_create_1k_dirs_with_files

    teardown; setup

    # Parallel Operations
    run_test "50 parallel workers (create/read/delete)" test_parallel_50_workers
    run_test "20 parallel writers to same dir"    test_parallel_create_same_dir

    teardown; setup

    # Deep Paths
    run_test "100-level deep path"                test_deep_path_100_levels

    teardown; setup

    # Mount Cycling
    run_test "10 mount/unmount cycles"            test_mount_unmount_cycles

    teardown; setup

    # Mixed Workload
    run_test "mixed concurrent workload"          test_mixed_workload

    teardown

    mkdir -p "$RESULTS_DIR"
    save_results_json "$RESULTS_DIR/nfs4_stress.json" "nfs4_stress"

    print_summary "NFSv4.0 Stress"
}

main "$@"
