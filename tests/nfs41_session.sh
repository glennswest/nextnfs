#!/bin/bash
# nfs41_session.sh — NFSv4.1 session protocol tests
#
# Tests: v4.1 mount, session establishment, recovery, multiple sessions
# Only runs if the server supports NFSv4.1; skips gracefully otherwise.
# Requires: NFS client support for vers=4.1

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

WORK41=""  # Set after mount
MOUNT41="/mnt/nfs-test-41"

# ── Helpers ──────────────────────────────────────────────────────────────────

mount_v41() {
    mkdir -p "$MOUNT41"
    local opts="vers=4.1,proto=tcp,port=${NFS_PORT},mountport=${NFS_PORT},soft,timeo=50,retrans=2"
    mount -t nfs4 -o "$opts" "${NFS_HOST}:${NFS_EXPORT}" "$MOUNT41" 2>/dev/null
}

unmount_v41() {
    if mountpoint -q "$MOUNT41" 2>/dev/null; then
        umount -l "$MOUNT41" 2>/dev/null || umount -f "$MOUNT41" 2>/dev/null || true
    fi
}

check_v41_support() {
    # Try mounting with v4.1 — if it fails, server doesn't support it
    if ! mount_v41; then
        return 1
    fi
    unmount_v41
    return 0
}

setup() {
    mount_v41 || return 1
    WORK41="$MOUNT41/session_tests"
    mkdir -p "$WORK41"
}

teardown() {
    rm -rf "$WORK41" 2>/dev/null || true
    unmount_v41
}

# ── Session Tests ────────────────────────────────────────────────────────────

test_v41_mount() {
    # Just verify the mount succeeded and we can list the directory
    ls "$MOUNT41" >/dev/null
}

test_v41_basic_io() {
    # EXCHANGE_ID + CREATE_SESSION happen implicitly during mount
    # SEQUENCE operations happen with each compound — test by doing I/O
    echo "v41 test data" > "$WORK41/session_file"
    local content
    content=$(cat "$WORK41/session_file")
    assert_eq "$content" "v41 test data" "read/write through v4.1 session"
}

test_v41_directory_ops() {
    mkdir -p "$WORK41/subdir/nested"
    touch "$WORK41/subdir/nested/file"
    local count
    count=$(find "$WORK41/subdir" -type f | wc -l)
    assert_eq "$(echo "$count" | tr -d ' ')" "1" "dir ops through v4.1"
    rm -rf "$WORK41/subdir"
}

test_v41_clean_unmount() {
    # DESTROY_SESSION happens during clean unmount
    echo "destroy test" > "$WORK41/destroy_file"
    unmount_v41
    sleep 1

    # Remount and verify data persisted
    mount_v41
    WORK41="$MOUNT41/session_tests"
    local content
    content=$(cat "$WORK41/destroy_file")
    assert_eq "$content" "destroy test" "data persists after session destroy/recreate"
}

test_v41_session_recovery() {
    # Write data, forcefully kill mount (simulating client crash), then remount
    echo "recovery data" > "$WORK41/recovery_file"

    # Force unmount (simulates abrupt disconnect)
    umount -f "$MOUNT41" 2>/dev/null || umount -l "$MOUNT41" 2>/dev/null || true
    sleep 2

    # Remount — server should handle session recovery
    mount_v41
    WORK41="$MOUNT41/session_tests"

    local content
    content=$(cat "$WORK41/recovery_file")
    assert_eq "$content" "recovery data" "data accessible after session recovery"
}

test_v41_multiple_sessions() {
    # Mount the same export at a second mount point
    local mount2="/mnt/nfs-test-41b"
    mkdir -p "$mount2"

    local opts="vers=4.1,proto=tcp,port=${NFS_PORT},mountport=${NFS_PORT},soft,timeo=50,retrans=2"
    if ! mount -t nfs4 -o "$opts" "${NFS_HOST}:${NFS_EXPORT}" "$mount2" 2>/dev/null; then
        echo "SKIP: cannot create second v4.1 mount"
        return 77
    fi

    # Write from session 1, read from session 2
    echo "multi session" > "$WORK41/multi_session_file"
    local content
    content=$(cat "$mount2/session_tests/multi_session_file")
    assert_eq "$content" "multi session" "cross-session data visibility"

    umount -l "$mount2" 2>/dev/null || true
    rmdir "$mount2" 2>/dev/null || true
}

# ── Run all tests ────────────────────────────────────────────────────────────

main() {
    suite_start "NFSv4.1 Session Protocol"

    # Check if v4.1 is supported before running any tests
    echo "  Checking NFSv4.1 support..."
    if ! check_v41_support; then
        echo -e "  ${YELLOW}Server does not support NFSv4.1 — skipping all session tests${RESET}"
        TESTS_TOTAL=6
        TESTS_SKIPPED=6
        local tests=("v4.1 mount" "v4.1 basic I/O" "v4.1 directory ops" "v4.1 clean unmount" "v4.1 session recovery" "v4.1 multiple sessions")
        for t in "${tests[@]}"; do
            SKIPPED_NAMES+=("$t")
            _record_result "$t" "skip" "server does not support NFSv4.1" "0"
        done
        SUITE_START_TIME=$(date +%s)
        mkdir -p "$RESULTS_DIR"
        save_results_json "$RESULTS_DIR/nfs41_session.json" "nfs41_session"
        print_summary "NFSv4.1 Session"
        return 0
    fi

    setup

    run_test "v4.1 mount"                         test_v41_mount
    run_test "v4.1 basic I/O"                     test_v41_basic_io
    run_test "v4.1 directory ops"                  test_v41_directory_ops
    run_test "v4.1 clean unmount"                  test_v41_clean_unmount

    teardown; setup

    run_test "v4.1 session recovery"              test_v41_session_recovery
    run_test "v4.1 multiple sessions"             test_v41_multiple_sessions

    teardown

    mkdir -p "$RESULTS_DIR"
    save_results_json "$RESULTS_DIR/nfs41_session.json" "nfs41_session"

    print_summary "NFSv4.1 Session"
}

main "$@"
