#!/bin/bash
# nfs4_edge.sh — NFSv4.0 edge cases and error handling tests
#
# Tests: error conditions, filehandle stability, concurrent access, locking
# Requires: mounted NFS export at $NFS_MOUNT

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

WORK="$NFS_MOUNT/edge_tests"

setup() {
    mkdir -p "$WORK"
}

teardown() {
    rm -rf "$WORK" 2>/dev/null || true
}

# ── Error Conditions ─────────────────────────────────────────────────────────

test_access_denied_read() {
    touch "$WORK/noperm"
    chmod 000 "$WORK/noperm"
    if [ "$(id -u)" -eq 0 ]; then
        chmod 644 "$WORK/noperm"
        echo "SKIP: root bypasses permissions"
        return 77
    fi
    if cat "$WORK/noperm" 2>/dev/null; then
        chmod 644 "$WORK/noperm"
        echo "FAIL: read succeeded on mode 000"
        return 1
    fi
    chmod 644 "$WORK/noperm"
}

test_access_denied_write() {
    touch "$WORK/noperm_w"
    chmod 444 "$WORK/noperm_w"
    if [ "$(id -u)" -eq 0 ]; then
        chmod 644 "$WORK/noperm_w"
        echo "SKIP: root bypasses permissions"
        return 77
    fi
    if echo "data" > "$WORK/noperm_w" 2>/dev/null; then
        chmod 644 "$WORK/noperm_w"
        echo "FAIL: write succeeded on mode 444"
        return 1
    fi
    chmod 644 "$WORK/noperm_w"
}

test_file_not_found() {
    ! stat "$WORK/nonexistent_file_xyz" 2>/dev/null
}

test_rmdir_not_empty() {
    mkdir -p "$WORK/notempty"
    touch "$WORK/notempty/file"
    ! rmdir "$WORK/notempty" 2>/dev/null
}

test_long_filename() {
    # NFS max filename is typically 255 bytes
    local name
    name=$(printf 'A%.0s' $(seq 1 255))
    touch "$WORK/$name"
    assert_file_exists "$WORK/$name" "255-char filename"
    rm -f "$WORK/$name"
}

test_very_long_filename_fails() {
    # 256+ char filename should fail
    local name
    name=$(printf 'B%.0s' $(seq 1 256))
    if touch "$WORK/$name" 2>/dev/null; then
        rm -f "$WORK/$name" 2>/dev/null
        echo "FAIL: 256-char filename unexpectedly succeeded"
        return 1
    fi
}

test_deep_path() {
    local path="$WORK/deep"
    local i
    for i in $(seq 1 50); do
        path="$path/d$i"
    done
    mkdir -p "$path"
    assert_is_dir "$path" "50-level deep path"
    # Verify we can create a file at the bottom
    echo "deep content" > "$path/file"
    assert_file_content "$path/file" "deep content" "file at depth 50"
    rm -rf "$WORK/deep"
}

test_special_chars_filename() {
    # Spaces, quotes, unicode
    touch "$WORK/file with spaces"
    assert_file_exists "$WORK/file with spaces" "spaces in filename"

    touch "$WORK/file'quote"
    assert_file_exists "$WORK/file'quote" "single quote in filename"

    touch "$WORK/file\"doublequote"
    assert_file_exists "$WORK/file\"doublequote" "double quote in filename"

    # Unicode
    touch "$WORK/filé_ñame_日本語"
    assert_file_exists "$WORK/filé_ñame_日本語" "unicode filename"
}

test_dot_files() {
    touch "$WORK/.hidden"
    assert_file_exists "$WORK/.hidden" "dot file created"
    local count
    count=$(ls -a "$WORK" | grep -c "^\\.hidden$")
    assert_eq "$count" "1" "dot file visible with ls -a"
}

# ── Filehandle Stability ─────────────────────────────────────────────────────

test_inode_survives_remount() {
    echo "inode test" > "$WORK/inode_file"
    local ino1
    ino1=$(file_inode "$WORK/inode_file")

    # Unmount and remount
    nfs_unmount "$NFS_MOUNT"
    sleep 1
    nfs_mount "$NFS_VERS" "$NFS_EXPORT" "$NFS_MOUNT"
    sleep 1

    # Re-create work dir reference (mount point changed)
    local ino2
    ino2=$(file_inode "$WORK/inode_file")
    assert_eq "$ino1" "$ino2" "inode stable across remount"
}

test_delete_open_file() {
    # NFS silly-rename: delete a file while it's held open
    echo "open file content" > "$WORK/open_delete"

    # Open file with a background reader, delete it, verify the fd still works
    (
        exec 3< "$WORK/open_delete"
        rm -f "$WORK/open_delete"
        # The fd should still be readable (NFS silly-rename)
        local content
        content=$(cat <&3)
        exec 3<&-
        if [ "$content" = "open file content" ]; then
            exit 0
        else
            exit 1
        fi
    )
}

test_rename_preserves_content() {
    echo "rename preserve" > "$WORK/rp_orig"
    local ino1
    ino1=$(file_inode "$WORK/rp_orig")
    mv "$WORK/rp_orig" "$WORK/rp_renamed"
    assert_file_content "$WORK/rp_renamed" "rename preserve" "content after rename"
}

# ── Concurrent Access ────────────────────────────────────────────────────────

test_concurrent_writes_same_file() {
    # Two processes writing to same file
    local file="$WORK/concurrent_write"
    (
        for i in $(seq 1 100); do
            echo "writer1 $i" >> "$file"
        done
    ) &
    local pid1=$!
    (
        for i in $(seq 1 100); do
            echo "writer2 $i" >> "$file"
        done
    ) &
    local pid2=$!
    wait "$pid1" "$pid2"

    local lines
    lines=$(wc -l < "$file")
    assert_eq "$(echo "$lines" | tr -d ' ')" "200" "200 total lines from 2 writers"
}

test_read_while_writing() {
    local file="$WORK/read_write_concurrent"
    echo "initial" > "$file"

    # Background writer
    (
        for i in $(seq 1 50); do
            echo "line $i" >> "$file"
            sleep 0.01
        done
    ) &
    local writer=$!

    # Reader
    local read_count=0
    for _ in $(seq 1 20); do
        cat "$file" >/dev/null 2>&1 && ((read_count++))
        sleep 0.02
    done
    wait "$writer"

    assert_gt "$read_count" "0" "reads succeeded while writing"
}

test_create_delete_race() {
    local dir="$WORK/race_dir"
    mkdir -p "$dir"

    # Two processes creating and deleting the same filename
    (
        for i in $(seq 1 50); do
            touch "$dir/racefile" 2>/dev/null || true
            rm -f "$dir/racefile" 2>/dev/null || true
        done
    ) &
    local pid1=$!
    (
        for i in $(seq 1 50); do
            touch "$dir/racefile" 2>/dev/null || true
            rm -f "$dir/racefile" 2>/dev/null || true
        done
    ) &
    local pid2=$!
    wait "$pid1" "$pid2"
    # Success = no crashes or hangs
}

# ── Locking ──────────────────────────────────────────────────────────────────

test_flock_exclusive() {
    local file="$WORK/flock_excl"
    echo "lock data" > "$file"

    # Take exclusive lock, try to get it from another process
    (
        flock -x -n 200 || exit 1
        sleep 2
    ) 200>"$file" &
    local holder=$!
    sleep 0.5

    # Second process should fail to get exclusive lock (non-blocking)
    local rc=0
    (flock -x -n 200 || exit 1) 200>"$file" 2>/dev/null || rc=$?
    wait "$holder" 2>/dev/null
    assert_ne "$rc" "0" "exclusive lock blocks second holder"
}

test_flock_shared() {
    local file="$WORK/flock_shared"
    echo "shared data" > "$file"

    # Two shared locks should both succeed
    (
        flock -s -n 200 || exit 1
        sleep 1
    ) 200>"$file" &
    local pid1=$!
    sleep 0.2

    local rc=0
    (flock -s -n 200 || exit 1) 200>"$file" 2>/dev/null || rc=$?
    wait "$pid1" 2>/dev/null
    assert_eq "$rc" "0" "shared locks coexist"
}

test_flock_release() {
    local file="$WORK/flock_release"
    echo "release data" > "$file"

    # Take and release lock
    (
        flock -x -n 200 || exit 1
        sleep 0.5
        # Lock auto-releases when subshell exits
    ) 200>"$file"

    # Now lock should be available
    local rc=0
    (flock -x -n 200 || exit 1) 200>"$file" 2>/dev/null || rc=$?
    assert_eq "$rc" "0" "lock available after release"
}

test_fcntl_byte_range_lock() {
    local file="$WORK/fcntl_lock"
    dd if=/dev/zero of="$file" bs=4096 count=1 2>/dev/null

    # Use python for fcntl byte-range locks if available
    if ! has_cmd python3; then
        echo "SKIP: python3 not available for fcntl test"
        return 77
    fi

    # Lock bytes 0-1023, try to lock same range from another process
    python3 -c "
import fcntl, struct, os, sys, time

fd = os.open('$file', os.O_RDWR)
# F_SETLK, type=F_WRLCK, whence=SEEK_SET, start=0, len=1024
lockdata = struct.pack('hhllhh', fcntl.F_WRLCK, 0, 0, 1024, 0, 0)
try:
    fcntl.fcntl(fd, fcntl.F_SETLK, lockdata)
except IOError:
    os.close(fd)
    sys.exit(1)

# Hold lock for 2 seconds
time.sleep(2)
os.close(fd)
" &
    local holder=$!
    sleep 0.5

    # Second process tries same range — should fail
    local rc=0
    python3 -c "
import fcntl, struct, os, sys

fd = os.open('$file', os.O_RDWR)
lockdata = struct.pack('hhllhh', fcntl.F_WRLCK, 0, 0, 1024, 0, 0)
try:
    fcntl.fcntl(fd, fcntl.F_SETLK, lockdata)
except IOError:
    os.close(fd)
    sys.exit(1)
os.close(fd)
" 2>/dev/null || rc=$?

    wait "$holder" 2>/dev/null
    assert_ne "$rc" "0" "byte-range lock conflict detected"
}

# ── Run all tests ────────────────────────────────────────────────────────────

main() {
    suite_start "NFSv4.0 Edge Cases & Error Handling"

    setup

    # Error Conditions
    run_test "access denied (read)"               test_access_denied_read
    run_test "access denied (write)"              test_access_denied_write
    run_test "file not found"                     test_file_not_found
    run_test "rmdir not empty"                    test_rmdir_not_empty
    run_test "long filename (255 chars)"          test_long_filename
    run_test "very long filename fails (256+)"    test_very_long_filename_fails
    run_test "deep path (50 levels)"              test_deep_path
    run_test "special chars in filename"          test_special_chars_filename
    run_test "dot files"                          test_dot_files

    teardown; setup

    # Filehandle Stability
    run_test "inode survives remount"             test_inode_survives_remount
    run_test "delete open file (silly-rename)"    test_delete_open_file
    run_test "rename preserves content"           test_rename_preserves_content

    teardown; setup

    # Concurrent Access
    run_test "concurrent writes same file"        test_concurrent_writes_same_file
    run_test "read while writing"                 test_read_while_writing
    run_test "create/delete race"                 test_create_delete_race

    teardown; setup

    # Locking
    run_test "flock exclusive"                    test_flock_exclusive
    run_test "flock shared"                       test_flock_shared
    run_test "flock release"                      test_flock_release
    run_test "fcntl byte-range lock"              test_fcntl_byte_range_lock

    teardown

    mkdir -p "$RESULTS_DIR"
    save_results_json "$RESULTS_DIR/nfs4_edge.json" "nfs4_edge"

    print_summary "NFSv4.0 Edge Cases"
}

main "$@"
