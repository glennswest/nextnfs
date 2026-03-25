#!/bin/bash
# nfs4_basic.sh — NFSv4.0 basic functional tests
#
# Tests: file ops, attributes, symlinks, hardlinks, read/write patterns, multi-export
# Requires: mounted NFS export at $NFS_MOUNT

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

# Working directory inside the NFS mount
WORK="$NFS_MOUNT/basic_tests"

setup() {
    mkdir -p "$WORK"
}

teardown() {
    rm -rf "$WORK" 2>/dev/null || true
}

# ── Namespace & File Operations ──────────────────────────────────────────────

test_create_file_touch() {
    touch "$WORK/touchfile"
    assert_file_exists "$WORK/touchfile" "touch created file"
}

test_create_file_redirect() {
    echo "hello world" > "$WORK/redirect_file"
    assert_file_exists "$WORK/redirect_file" "redirect created file"
    assert_file_content "$WORK/redirect_file" "hello world" "redirect content"
}

test_read_file_content() {
    echo "read test data 12345" > "$WORK/readtest"
    local content
    content=$(cat "$WORK/readtest")
    assert_eq "$content" "read test data 12345" "read content matches"
}

test_sha256_integrity() {
    echo "integrity check payload" > "$WORK/sha256test"
    local expected actual
    expected=$(echo "integrity check payload" | sha256sum | awk '{print $1}')
    actual=$(sha256sum "$WORK/sha256test" | awk '{print $1}')
    assert_eq "$actual" "$expected" "sha256 integrity"
}

test_create_nested_dirs() {
    mkdir -p "$WORK/a/b/c/d/e"
    assert_is_dir "$WORK/a/b/c/d/e" "nested dirs created"
}

test_list_directory() {
    mkdir -p "$WORK/listdir"
    touch "$WORK/listdir/file1" "$WORK/listdir/file2" "$WORK/listdir/file3"
    local count
    count=$(ls "$WORK/listdir" | wc -l)
    assert_eq "$(echo "$count" | tr -d ' ')" "3" "ls shows 3 files"
}

test_find_recursive() {
    mkdir -p "$WORK/findtest/sub1/sub2"
    touch "$WORK/findtest/a.txt" "$WORK/findtest/sub1/b.txt" "$WORK/findtest/sub1/sub2/c.txt"
    local count
    count=$(find "$WORK/findtest" -name "*.txt" | wc -l)
    assert_eq "$(echo "$count" | tr -d ' ')" "3" "find locates 3 files"
}

test_remove_file() {
    touch "$WORK/removeme"
    assert_file_exists "$WORK/removeme" "file created"
    rm "$WORK/removeme"
    assert_file_not_exists "$WORK/removeme" "file removed"
}

test_remove_directory_recursive() {
    mkdir -p "$WORK/rmdir/sub/deep"
    touch "$WORK/rmdir/sub/deep/file"
    rm -rf "$WORK/rmdir"
    assert_file_not_exists "$WORK/rmdir" "recursive rm"
}

test_rename_file() {
    echo "rename content" > "$WORK/oldname"
    mv "$WORK/oldname" "$WORK/newname"
    assert_file_not_exists "$WORK/oldname" "old name gone"
    assert_file_content "$WORK/newname" "rename content" "new name has content"
}

test_rename_across_dirs() {
    mkdir -p "$WORK/dir_a" "$WORK/dir_b"
    echo "cross dir" > "$WORK/dir_a/moveme"
    mv "$WORK/dir_a/moveme" "$WORK/dir_b/moveme"
    assert_file_not_exists "$WORK/dir_a/moveme" "source gone"
    assert_file_content "$WORK/dir_b/moveme" "cross dir" "dest has content"
}

test_rename_directory() {
    mkdir -p "$WORK/rename_dir_old/child"
    touch "$WORK/rename_dir_old/child/f"
    mv "$WORK/rename_dir_old" "$WORK/rename_dir_new"
    assert_file_not_exists "$WORK/rename_dir_old" "old dir gone"
    assert_file_exists "$WORK/rename_dir_new/child/f" "contents preserved"
}

test_copy_file() {
    echo "copy source" > "$WORK/copy_src"
    cp "$WORK/copy_src" "$WORK/copy_dst"
    assert_file_content "$WORK/copy_dst" "copy source" "copy content"
}

test_copy_recursive() {
    mkdir -p "$WORK/cpr_src/sub"
    echo "deep" > "$WORK/cpr_src/sub/file"
    cp -r "$WORK/cpr_src" "$WORK/cpr_dst"
    assert_file_content "$WORK/cpr_dst/sub/file" "deep" "recursive copy"
}

# ── File Attributes ──────────────────────────────────────────────────────────

test_chmod() {
    touch "$WORK/chmodfile"
    chmod 755 "$WORK/chmodfile"
    local mode
    mode=$(file_mode "$WORK/chmodfile")
    assert_eq "$mode" "755" "chmod 755"

    chmod 644 "$WORK/chmodfile"
    mode=$(file_mode "$WORK/chmodfile")
    assert_eq "$mode" "644" "chmod 644"
}

test_chown() {
    # Only works as root
    if [ "$(id -u)" -ne 0 ]; then
        echo "SKIP: not root"
        return 77
    fi
    touch "$WORK/chownfile"
    chown 1000:1000 "$WORK/chownfile"
    local uid gid
    uid=$(stat -c %u "$WORK/chownfile")
    gid=$(stat -c %g "$WORK/chownfile")
    assert_eq "$uid" "1000" "chown uid"
    assert_eq "$gid" "1000" "chown gid"
}

test_timestamps_mtime() {
    touch "$WORK/tsfile"
    local mtime1
    mtime1=$(stat -c %Y "$WORK/tsfile")
    sleep 1
    echo "update" >> "$WORK/tsfile"
    local mtime2
    mtime2=$(stat -c %Y "$WORK/tsfile")
    assert_ne "$mtime2" "$mtime1" "mtime changed after write"
}

test_stat_size() {
    dd if=/dev/zero of="$WORK/sizetest" bs=4096 count=1 2>/dev/null
    local size
    size=$(file_size "$WORK/sizetest")
    assert_eq "$size" "4096" "file size is 4096"
}

test_stat_nlink_file() {
    touch "$WORK/nlinkfile"
    local nlink
    nlink=$(file_nlink "$WORK/nlinkfile")
    assert_eq "$nlink" "1" "regular file nlink=1"
}

test_stat_type() {
    touch "$WORK/typefile"
    mkdir -p "$WORK/typedir"
    assert_eq "$(file_type "$WORK/typefile")" "file" "type=file"
    assert_eq "$(file_type "$WORK/typedir")" "directory" "type=directory"
}

# ── Symlinks & Hard Links ────────────────────────────────────────────────────

test_symlink_create_read() {
    echo "symlink target data" > "$WORK/sym_target"
    ln -s "$WORK/sym_target" "$WORK/sym_link"
    assert_is_symlink "$WORK/sym_link" "is symlink"
    local content
    content=$(cat "$WORK/sym_link")
    assert_eq "$content" "symlink target data" "read through symlink"
}

test_symlink_readlink() {
    echo "x" > "$WORK/rl_target"
    ln -s "$WORK/rl_target" "$WORK/rl_link"
    local target
    target=$(readlink "$WORK/rl_link")
    assert_eq "$target" "$WORK/rl_target" "readlink"
}

test_hardlink_create() {
    echo "hardlink data" > "$WORK/hl_original"
    ln "$WORK/hl_original" "$WORK/hl_link"
    # Both should have nlink=2
    local nlink
    nlink=$(file_nlink "$WORK/hl_original")
    assert_eq "$nlink" "2" "nlink=2 after hardlink"
    # Same content
    local content
    content=$(cat "$WORK/hl_link")
    assert_eq "$content" "hardlink data" "hardlink content"
}

test_hardlink_remove_decrement() {
    echo "hl rm" > "$WORK/hlrm_orig"
    ln "$WORK/hlrm_orig" "$WORK/hlrm_link"
    assert_eq "$(file_nlink "$WORK/hlrm_orig")" "2" "nlink=2"
    rm "$WORK/hlrm_link"
    assert_eq "$(file_nlink "$WORK/hlrm_orig")" "1" "nlink=1 after rm"
}

test_hardlink_same_inode() {
    echo "same inode" > "$WORK/hli_orig"
    ln "$WORK/hli_orig" "$WORK/hli_link"
    local ino1 ino2
    ino1=$(file_inode "$WORK/hli_orig")
    ino2=$(file_inode "$WORK/hli_link")
    assert_eq "$ino1" "$ino2" "hardlinks share inode"
}

# ── Read/Write Patterns ─────────────────────────────────────────────────────

test_write_read_4k() {
    dd if=/dev/urandom of="$WORK/rw_4k" bs=4096 count=1 2>/dev/null
    local md5_orig md5_read
    md5_orig=$(md5sum "$WORK/rw_4k" | awk '{print $1}')
    cp "$WORK/rw_4k" "$WORK/rw_4k_copy"
    md5_read=$(md5sum "$WORK/rw_4k_copy" | awk '{print $1}')
    assert_eq "$md5_read" "$md5_orig" "4K write/read integrity"
}

test_write_read_1m() {
    dd if=/dev/urandom of="$WORK/rw_1m" bs=1M count=1 2>/dev/null
    local md5_orig md5_read
    md5_orig=$(md5sum "$WORK/rw_1m" | awk '{print $1}')
    cp "$WORK/rw_1m" "$WORK/rw_1m_copy"
    md5_read=$(md5sum "$WORK/rw_1m_copy" | awk '{print $1}')
    assert_eq "$md5_read" "$md5_orig" "1M write/read integrity"
}

test_write_read_100m() {
    dd if=/dev/urandom of="$WORK/rw_100m" bs=1M count=100 2>/dev/null
    local md5_orig md5_read
    md5_orig=$(md5sum "$WORK/rw_100m" | awk '{print $1}')
    cp "$WORK/rw_100m" "$WORK/rw_100m_copy"
    md5_read=$(md5sum "$WORK/rw_100m_copy" | awk '{print $1}')
    assert_eq "$md5_read" "$md5_orig" "100M write/read integrity"
    rm -f "$WORK/rw_100m" "$WORK/rw_100m_copy"
}

test_random_offset_write() {
    # Write 4K at offset 1M
    dd if=/dev/zero of="$WORK/offset_file" bs=4096 count=1 seek=256 2>/dev/null
    local size
    size=$(file_size "$WORK/offset_file")
    # Should be at least 1M + 4K (offset 256*4096 + 4096)
    assert_gt "$size" "1048576" "offset write creates sparse/extended file"
}

test_append_write() {
    echo "line1" > "$WORK/append_file"
    echo "line2" >> "$WORK/append_file"
    echo "line3" >> "$WORK/append_file"
    local lines
    lines=$(wc -l < "$WORK/append_file")
    assert_eq "$(echo "$lines" | tr -d ' ')" "3" "append produced 3 lines"
    local content
    content=$(cat "$WORK/append_file")
    assert_contains "$content" "line1" "has line1"
    assert_contains "$content" "line3" "has line3"
}

test_overwrite_file() {
    echo "original" > "$WORK/overwrite_file"
    echo "replaced" > "$WORK/overwrite_file"
    assert_file_content "$WORK/overwrite_file" "replaced" "overwrite"
}

test_zero_byte_file() {
    touch "$WORK/empty_file"
    local size
    size=$(file_size "$WORK/empty_file")
    assert_eq "$size" "0" "zero-byte file"
    local content
    content=$(cat "$WORK/empty_file")
    assert_eq "$content" "" "empty content"
}

test_large_file_integrity() {
    # Write 1G file and verify integrity
    dd if=/dev/urandom of="$WORK/large_src" bs=1M count=256 2>/dev/null
    local md5_src
    md5_src=$(md5sum "$WORK/large_src" | awk '{print $1}')
    cp "$WORK/large_src" "$WORK/large_dst"
    local md5_dst
    md5_dst=$(md5sum "$WORK/large_dst" | awk '{print $1}')
    assert_eq "$md5_dst" "$md5_src" "256M file integrity"
    rm -f "$WORK/large_src" "$WORK/large_dst"
}

# ── Multi-Export Tests ───────────────────────────────────────────────────────

test_multi_export_isolation() {
    # This test requires two separate exports mounted at different points
    # Skip if second mount point is not available
    local mount2="/mnt/nfs-test2"
    if ! mountpoint -q "$mount2" 2>/dev/null; then
        echo "SKIP: second export not mounted at $mount2"
        return 77
    fi

    echo "export1 data" > "$NFS_MOUNT/isolation_test"
    # Should not appear in export2
    if [ -f "$mount2/isolation_test" ]; then
        echo "FAIL: file leaked across exports"
        return 1
    fi
    rm -f "$NFS_MOUNT/isolation_test"
}

# ── Run all tests ────────────────────────────────────────────────────────────

main() {
    suite_start "NFSv4.0 Basic Functional Tests"

    setup

    # Namespace & File Operations
    run_test "create file (touch)"               test_create_file_touch
    run_test "create file (redirect)"             test_create_file_redirect
    run_test "read file content"                  test_read_file_content
    run_test "sha256 integrity"                   test_sha256_integrity
    run_test "create nested dirs"                 test_create_nested_dirs
    run_test "list directory"                     test_list_directory
    run_test "find recursive"                     test_find_recursive
    run_test "remove file"                        test_remove_file
    run_test "remove directory recursive"         test_remove_directory_recursive
    run_test "rename file"                        test_rename_file
    run_test "rename across dirs"                 test_rename_across_dirs
    run_test "rename directory"                   test_rename_directory
    run_test "copy file"                          test_copy_file
    run_test "copy recursive"                     test_copy_recursive

    teardown; setup

    # File Attributes
    run_test "chmod"                              test_chmod
    run_test "chown"                              test_chown
    run_test "mtime updates after write"          test_timestamps_mtime
    run_test "stat size"                          test_stat_size
    run_test "stat nlink (file)"                  test_stat_nlink_file
    run_test "stat type"                          test_stat_type

    teardown; setup

    # Symlinks & Hard Links
    run_test "symlink create and read"            test_symlink_create_read
    run_test "readlink"                           test_symlink_readlink
    run_test "hardlink create"                    test_hardlink_create
    run_test "hardlink remove decrements nlink"   test_hardlink_remove_decrement
    run_test "hardlinks share inode"              test_hardlink_same_inode

    teardown; setup

    # Read/Write Patterns
    run_test "write/read 4K"                      test_write_read_4k
    run_test "write/read 1M"                      test_write_read_1m
    run_test "write/read 100M"                    test_write_read_100m
    run_test "random offset write"                test_random_offset_write
    run_test "append writes"                      test_append_write
    run_test "overwrite file"                     test_overwrite_file
    run_test "zero-byte file"                     test_zero_byte_file
    run_test "large file integrity (256M)"        test_large_file_integrity

    teardown; setup

    # Multi-Export
    run_test "multi-export isolation"             test_multi_export_isolation

    teardown

    # Save results
    mkdir -p "$RESULTS_DIR"
    save_results_json "$RESULTS_DIR/nfs4_basic.json" "nfs4_basic"

    print_summary "NFSv4.0 Basic"
}

main "$@"
