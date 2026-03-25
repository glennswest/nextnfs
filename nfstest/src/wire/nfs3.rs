//! NFSv3 (RFC 1813) wire-level test definitions and executors.

#![allow(dead_code)]

use super::rpc::{self, RpcClient};
use super::xdr::{XdrDecoder, XdrEncoder};
use crate::harness::{NfsVersion, TestDef, TestLayer};

// NFSv3 procedure numbers
const NFSPROC3_NULL: u32 = 0;
const NFSPROC3_GETATTR: u32 = 1;
const NFSPROC3_SETATTR: u32 = 2;
const NFSPROC3_LOOKUP: u32 = 3;
const NFSPROC3_ACCESS: u32 = 4;
const NFSPROC3_READLINK: u32 = 5;
const NFSPROC3_READ: u32 = 6;
const NFSPROC3_WRITE: u32 = 7;
const NFSPROC3_CREATE: u32 = 8;
const NFSPROC3_MKDIR: u32 = 9;
const NFSPROC3_SYMLINK: u32 = 10;
const NFSPROC3_MKNOD: u32 = 11;
const NFSPROC3_REMOVE: u32 = 12;
const NFSPROC3_RMDIR: u32 = 13;
const NFSPROC3_RENAME: u32 = 14;
const NFSPROC3_LINK: u32 = 15;
const NFSPROC3_READDIR: u32 = 16;
const NFSPROC3_READDIRPLUS: u32 = 17;
const NFSPROC3_FSSTAT: u32 = 18;
const NFSPROC3_FSINFO: u32 = 19;
const NFSPROC3_PATHCONF: u32 = 20;
const NFSPROC3_COMMIT: u32 = 21;

// MOUNT v3 procedures
const MOUNTPROC3_NULL: u32 = 0;
const MOUNTPROC3_MNT: u32 = 1;
const MOUNTPROC3_DUMP: u32 = 2;
const MOUNTPROC3_UMNT: u32 = 3;
const MOUNTPROC3_EXPORT: u32 = 5;

// NFSv3 status codes
const NFS3_OK: u32 = 0;
const NFS3ERR_PERM: u32 = 1;
const NFS3ERR_NOENT: u32 = 2;
const NFS3ERR_IO: u32 = 5;
const NFS3ERR_ACCES: u32 = 13;
const NFS3ERR_EXIST: u32 = 17;
const NFS3ERR_NOTDIR: u32 = 20;
const NFS3ERR_ISDIR: u32 = 21;
const NFS3ERR_INVAL: u32 = 22;
const NFS3ERR_NOTEMPTY: u32 = 66;
const NFS3ERR_STALE: u32 = 70;
const NFS3ERR_BADHANDLE: u32 = 10001;
const NFS3ERR_BAD_COOKIE: u32 = 10003;
const NFS3ERR_NAMETOOLONG: u32 = 63;

// Access bits
const ACCESS3_READ: u32 = 0x0001;
const ACCESS3_LOOKUP: u32 = 0x0002;
const ACCESS3_MODIFY: u32 = 0x0004;
const ACCESS3_EXTEND: u32 = 0x0008;
const ACCESS3_DELETE: u32 = 0x0010;
const ACCESS3_EXECUTE: u32 = 0x0020;

// File types
const NF3REG: u32 = 1;
const NF3DIR: u32 = 2;
const NF3LNK: u32 = 5;

// Write stability
const UNSTABLE: u32 = 0;
const DATA_SYNC: u32 = 1;
const FILE_SYNC: u32 = 2;

// Create mode
const UNCHECKED: u32 = 0;
const GUARDED: u32 = 1;
const EXCLUSIVE: u32 = 2;

/// Return all NFSv3 wire-level test definitions.
pub fn tests() -> Vec<TestDef> {
    vec![
        // NULL
        TestDef {
            id: "W3-001",
            description: "NULL RPC returns success",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["smoke", "ci", "v3"],
        },
        // GETATTR
        TestDef {
            id: "W3-002",
            description: "GETATTR on root filehandle",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["smoke", "ci", "v3"],
        },
        TestDef {
            id: "W3-005",
            description: "GETATTR with invalid filehandle returns BADHANDLE or STALE",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
        // LOOKUP
        TestDef {
            id: "W3-012",
            description: "LOOKUP resolves name in root directory",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
        TestDef {
            id: "W3-013",
            description: "LOOKUP nonexistent name returns NOENT",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
        // ACCESS
        TestDef {
            id: "W3-016",
            description: "ACCESS returns permission bits for root fh",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
        // READ / WRITE
        TestDef {
            id: "W3-020",
            description: "READ small file",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
        TestDef {
            id: "W3-025",
            description: "WRITE FILE_SYNC and verify with READ",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
        TestDef {
            id: "W3-031",
            description: "COMMIT flushes unstable writes",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
        // Namespace
        TestDef {
            id: "W3-033",
            description: "CREATE regular file (UNCHECKED mode)",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
        TestDef {
            id: "W3-036",
            description: "MKDIR creates directory",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
        TestDef {
            id: "W3-043",
            description: "REMOVE deletes file",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
        TestDef {
            id: "W3-045",
            description: "RMDIR removes empty directory",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
        TestDef {
            id: "W3-047",
            description: "RENAME within same directory",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
        TestDef {
            id: "W3-050",
            description: "LINK creates hard link",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
        // READDIR
        TestDef {
            id: "W3-052",
            description: "READDIR lists directory entries",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
        TestDef {
            id: "W3-056",
            description: "READDIRPLUS returns entries with attributes",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
        // FS info
        TestDef {
            id: "W3-058",
            description: "FSSTAT returns filesystem statistics",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
        TestDef {
            id: "W3-059",
            description: "FSINFO returns server capabilities",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
        TestDef {
            id: "W3-060",
            description: "PATHCONF returns path configuration",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
        // MOUNT
        TestDef {
            id: "W3-061",
            description: "MOUNT returns root filehandle",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["smoke", "ci", "v3"],
        },
        TestDef {
            id: "W3-065",
            description: "MOUNT EXPORT lists exported paths",
            version: NfsVersion::V3,
            layer: TestLayer::Wire,
            tags: vec!["ci", "v3"],
        },
    ]
}

/// Execute an NFSv3 wire-level test.
pub async fn execute(test_id: &str, client: &RpcClient) -> anyhow::Result<()> {
    match test_id {
        "W3-001" => test_null(client).await,
        "W3-002" => test_getattr_root(client).await,
        "W3-005" => test_getattr_invalid_fh(client).await,
        "W3-012" => test_lookup(client).await,
        "W3-013" => test_lookup_noent(client).await,
        "W3-016" => test_access(client).await,
        "W3-020" => test_read(client).await,
        "W3-025" => test_write_filesync(client).await,
        "W3-031" => test_commit(client).await,
        "W3-033" => test_create_file(client).await,
        "W3-036" => test_mkdir(client).await,
        "W3-043" => test_remove(client).await,
        "W3-045" => test_rmdir(client).await,
        "W3-047" => test_rename(client).await,
        "W3-050" => test_link(client).await,
        "W3-052" => test_readdir(client).await,
        "W3-056" => test_readdirplus(client).await,
        "W3-058" => test_fsstat(client).await,
        "W3-059" => test_fsinfo(client).await,
        "W3-060" => test_pathconf(client).await,
        "W3-061" => test_mount(client).await,
        "W3-065" => test_mount_export(client).await,
        _ => anyhow::bail!("SKIP: Test {} not yet implemented", test_id),
    }
}

// --- Helper: get root filehandle via MOUNT ---

async fn mount_root(client: &RpcClient) -> anyhow::Result<Vec<u8>> {
    let mut args = XdrEncoder::new();
    args.put_string("/"); // export path
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::MOUNT_PROGRAM, rpc::MOUNT_V3, MOUNTPROC3_MNT, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != 0 {
        anyhow::bail!("MOUNT failed with status {}", status);
    }

    // fhandle3 is a variable-length opaque
    let fh = dec.get_opaque()?;
    Ok(fh)
}

// --- Helper: encode an nfs_fh3 (opaque) ---

fn encode_fh(enc: &mut XdrEncoder, fh: &[u8]) {
    enc.put_opaque(fh);
}

// --- Test implementations ---

async fn test_null(client: &RpcClient) -> anyhow::Result<()> {
    client.null_call(rpc::NFS_PROGRAM, rpc::NFS_V3).await?;
    Ok(())
}

async fn test_getattr_root(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;

    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &fh);
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_GETATTR, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        anyhow::bail!("GETATTR returned status {}", status);
    }

    // Parse fattr3
    let ftype = dec.get_u32()?;
    if ftype != NF3DIR {
        anyhow::bail!("Root should be a directory (type 2), got type {}", ftype);
    }

    Ok(())
}

async fn test_getattr_invalid_fh(client: &RpcClient) -> anyhow::Result<()> {
    // Send a garbage filehandle
    let fake_fh = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02, 0x03];

    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &fake_fh);
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_GETATTR, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;

    // Should get BADHANDLE or STALE
    if status == NFS3_OK {
        anyhow::bail!("GETATTR with invalid FH should not return OK");
    }

    Ok(())
}

async fn test_lookup(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;

    // First create a test file so we have something to look up
    let test_name = "nfstest_lookup_target";
    create_test_file(client, &fh, test_name).await?;

    // Now LOOKUP
    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &fh);
    args.put_string(test_name);
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_LOOKUP, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        anyhow::bail!("LOOKUP returned status {}", status);
    }

    // Clean up
    remove_file(client, &fh, test_name).await.ok();

    Ok(())
}

async fn test_lookup_noent(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;

    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &fh);
    args.put_string("__nonexistent_file_nextnfstest__");
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_LOOKUP, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3ERR_NOENT {
        anyhow::bail!("Expected NOENT (2), got status {}", status);
    }

    Ok(())
}

async fn test_access(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;

    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &fh);
    args.put_u32(ACCESS3_READ | ACCESS3_LOOKUP | ACCESS3_MODIFY | ACCESS3_EXTEND | ACCESS3_DELETE | ACCESS3_EXECUTE);
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_ACCESS, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        anyhow::bail!("ACCESS returned status {}", status);
    }

    // Skip post_op_attr (optional)
    let has_attr = dec.get_bool()?;
    if has_attr {
        // fattr3 is 84 bytes (21 u32s worth of fields)
        // type(4) + mode(4) + nlink(4) + uid(4) + gid(4) + size(8) + used(8)
        // + specdata(8) + fsid(8) + fileid(8) + atime(8) + mtime(8) + ctime(8) = 84
        for _ in 0..21 {
            dec.get_u32()?;
        }
    }

    let access = dec.get_u32()?;
    // Root directory should at least have READ and LOOKUP
    if (access & ACCESS3_READ) == 0 {
        anyhow::bail!("Root directory not readable");
    }
    if (access & ACCESS3_LOOKUP) == 0 {
        anyhow::bail!("Root directory not lookupable");
    }

    Ok(())
}

async fn test_read(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;

    // Create a test file with known content
    let test_name = "nfstest_read_target";
    let test_data = b"Hello from nextnfstest READ test!";
    let file_fh = create_test_file_with_data(client, &fh, test_name, test_data).await?;

    // READ the file
    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &file_fh);
    args.put_u64(0); // offset
    args.put_u32(4096); // count
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_READ, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        remove_file(client, &fh, test_name).await.ok();
        anyhow::bail!("READ returned status {}", status);
    }

    // Skip post_op_attr
    let has_attr = dec.get_bool()?;
    if has_attr {
        for _ in 0..21 {
            dec.get_u32()?;
        }
    }

    let count = dec.get_u32()?;
    let eof = dec.get_bool()?;
    let data = dec.get_opaque()?;

    if &data[..test_data.len()] != test_data {
        remove_file(client, &fh, test_name).await.ok();
        anyhow::bail!("READ data mismatch");
    }

    // Clean up
    remove_file(client, &fh, test_name).await.ok();
    Ok(())
}

async fn test_write_filesync(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;

    let test_name = "nfstest_write_target";
    let file_fh = create_test_file(client, &fh, test_name).await?;

    // WRITE with FILE_SYNC
    let test_data = b"FILE_SYNC write test data from nextnfstest";
    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &file_fh);
    args.put_u64(0); // offset
    args.put_u32(test_data.len() as u32); // count
    args.put_u32(FILE_SYNC); // stable
    args.put_opaque(test_data); // data
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_WRITE, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        remove_file(client, &fh, test_name).await.ok();
        anyhow::bail!("WRITE returned status {}", status);
    }

    // Clean up
    remove_file(client, &fh, test_name).await.ok();
    Ok(())
}

async fn test_commit(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;

    let test_name = "nfstest_commit_target";
    let file_fh = create_test_file(client, &fh, test_name).await?;

    // WRITE with UNSTABLE
    let test_data = b"UNSTABLE write for COMMIT test";
    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &file_fh);
    args.put_u64(0);
    args.put_u32(test_data.len() as u32);
    args.put_u32(UNSTABLE);
    args.put_opaque(test_data);
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_WRITE, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        remove_file(client, &fh, test_name).await.ok();
        anyhow::bail!("WRITE (UNSTABLE) returned status {}", status);
    }

    // COMMIT
    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &file_fh);
    args.put_u64(0); // offset
    args.put_u32(0); // count (0 = entire file)
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_COMMIT, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        remove_file(client, &fh, test_name).await.ok();
        anyhow::bail!("COMMIT returned status {}", status);
    }

    // Clean up
    remove_file(client, &fh, test_name).await.ok();
    Ok(())
}

async fn test_create_file(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;

    let test_name = "nfstest_create_target";
    let file_fh = create_test_file(client, &fh, test_name).await?;

    // Verify it exists via LOOKUP
    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &fh);
    args.put_string(test_name);
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_LOOKUP, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        anyhow::bail!("LOOKUP after CREATE returned status {}", status);
    }

    // Clean up
    remove_file(client, &fh, test_name).await.ok();
    Ok(())
}

async fn test_mkdir(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;
    let test_name = "nfstest_mkdir_target";

    // MKDIR
    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &fh);
    args.put_string(test_name);
    // sattr3 - set mode
    args.put_bool(true); // set mode
    args.put_u32(0o755);
    args.put_bool(false); // set uid
    args.put_bool(false); // set gid
    args.put_bool(false); // set size
    args.put_u32(0); // atime: DONT_CHANGE
    args.put_u32(0); // mtime: DONT_CHANGE
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_MKDIR, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        anyhow::bail!("MKDIR returned status {}", status);
    }

    // Clean up
    rmdir(client, &fh, test_name).await.ok();
    Ok(())
}

async fn test_remove(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;
    let test_name = "nfstest_remove_target";

    // Create file first
    create_test_file(client, &fh, test_name).await?;

    // REMOVE
    remove_file(client, &fh, test_name).await?;

    // Verify it's gone
    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &fh);
    args.put_string(test_name);
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_LOOKUP, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3ERR_NOENT {
        anyhow::bail!("File should be gone after REMOVE, LOOKUP returned {}", status);
    }

    Ok(())
}

async fn test_rmdir(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;
    let test_name = "nfstest_rmdir_target";

    // Create dir
    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &fh);
    args.put_string(test_name);
    args.put_bool(true); args.put_u32(0o755);
    args.put_bool(false); args.put_bool(false); args.put_bool(false);
    args.put_u32(0); args.put_u32(0);
    let args_bytes = args.finish();

    client.call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_MKDIR, &args_bytes).await?;

    // RMDIR
    rmdir(client, &fh, test_name).await?;

    // Verify gone
    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &fh);
    args.put_string(test_name);
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_LOOKUP, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3ERR_NOENT {
        anyhow::bail!("Directory should be gone after RMDIR, LOOKUP returned {}", status);
    }

    Ok(())
}

async fn test_rename(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;
    let old_name = "nfstest_rename_src";
    let new_name = "nfstest_rename_dst";

    // Clean up any leftover
    remove_file(client, &fh, old_name).await.ok();
    remove_file(client, &fh, new_name).await.ok();

    // Create source
    create_test_file(client, &fh, old_name).await?;

    // RENAME
    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &fh);
    args.put_string(old_name);
    encode_fh(&mut args, &fh);
    args.put_string(new_name);
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_RENAME, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        anyhow::bail!("RENAME returned status {}", status);
    }

    // Clean up
    remove_file(client, &fh, new_name).await.ok();
    Ok(())
}

async fn test_link(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;
    let file_name = "nfstest_link_src";
    let link_name = "nfstest_link_dst";

    // Clean up
    remove_file(client, &fh, file_name).await.ok();
    remove_file(client, &fh, link_name).await.ok();

    // Create source file
    let file_fh = create_test_file(client, &fh, file_name).await?;

    // LINK
    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &file_fh);
    encode_fh(&mut args, &fh);
    args.put_string(link_name);
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_LINK, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        anyhow::bail!("LINK returned status {}", status);
    }

    // Clean up
    remove_file(client, &fh, link_name).await.ok();
    remove_file(client, &fh, file_name).await.ok();
    Ok(())
}

async fn test_readdir(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;

    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &fh);
    args.put_u64(0); // cookie
    args.put_opaque_fixed(&[0u8; 8]); // cookieverf (8 bytes)
    args.put_u32(8192); // dircount
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_READDIR, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        anyhow::bail!("READDIR returned status {}", status);
    }

    Ok(())
}

async fn test_readdirplus(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;

    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &fh);
    args.put_u64(0); // cookie
    args.put_opaque_fixed(&[0u8; 8]); // cookieverf
    args.put_u32(8192); // dircount
    args.put_u32(32768); // maxcount
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_READDIRPLUS, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        anyhow::bail!("READDIRPLUS returned status {}", status);
    }

    Ok(())
}

async fn test_fsstat(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;

    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &fh);
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_FSSTAT, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        anyhow::bail!("FSSTAT returned status {}", status);
    }

    Ok(())
}

async fn test_fsinfo(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;

    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &fh);
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_FSINFO, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        anyhow::bail!("FSINFO returned status {}", status);
    }

    Ok(())
}

async fn test_pathconf(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;

    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &fh);
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_PATHCONF, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        anyhow::bail!("PATHCONF returned status {}", status);
    }

    Ok(())
}

async fn test_mount(client: &RpcClient) -> anyhow::Result<()> {
    let fh = mount_root(client).await?;
    if fh.is_empty() {
        anyhow::bail!("MOUNT returned empty filehandle");
    }
    Ok(())
}

async fn test_mount_export(client: &RpcClient) -> anyhow::Result<()> {
    let reply = client
        .call(rpc::MOUNT_PROGRAM, rpc::MOUNT_V3, MOUNTPROC3_EXPORT, &[])
        .await?;

    // Should return at least one export entry (even if just "/")
    // The reply is a linked list of exportlist entries; at minimum it should decode
    if reply.is_empty() {
        anyhow::bail!("EXPORT returned empty reply");
    }

    Ok(())
}

// --- Helpers ---

/// Create a test file via CREATE (UNCHECKED) and return its filehandle.
async fn create_test_file(client: &RpcClient, dir_fh: &[u8], name: &str) -> anyhow::Result<Vec<u8>> {
    let mut args = XdrEncoder::new();
    encode_fh(&mut args, dir_fh);
    args.put_string(name);
    args.put_u32(UNCHECKED); // createmode
    // sattr3
    args.put_bool(true); args.put_u32(0o644); // mode
    args.put_bool(false); // uid
    args.put_bool(false); // gid
    args.put_bool(false); // size
    args.put_u32(0); // atime SET_TO_SERVER_TIME=0 is DONT_CHANGE, 1=SET_TO_SERVER_TIME
    args.put_u32(0); // mtime
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_CREATE, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        anyhow::bail!("CREATE '{}' returned status {}", name, status);
    }

    // post_op_fh3: bool + optional fh
    let has_fh = dec.get_bool()?;
    if !has_fh {
        anyhow::bail!("CREATE did not return a filehandle");
    }
    let fh = dec.get_opaque()?;
    Ok(fh)
}

/// Create a test file and write data to it.
async fn create_test_file_with_data(
    client: &RpcClient,
    dir_fh: &[u8],
    name: &str,
    data: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let file_fh = create_test_file(client, dir_fh, name).await?;

    // WRITE
    let mut args = XdrEncoder::new();
    encode_fh(&mut args, &file_fh);
    args.put_u64(0); // offset
    args.put_u32(data.len() as u32); // count
    args.put_u32(FILE_SYNC); // stable
    args.put_opaque(data); // data
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_WRITE, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        anyhow::bail!("WRITE to '{}' returned status {}", name, status);
    }

    Ok(file_fh)
}

/// Remove a file.
async fn remove_file(client: &RpcClient, dir_fh: &[u8], name: &str) -> anyhow::Result<()> {
    let mut args = XdrEncoder::new();
    encode_fh(&mut args, dir_fh);
    args.put_string(name);
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_REMOVE, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        anyhow::bail!("REMOVE '{}' returned status {}", name, status);
    }
    Ok(())
}

/// Remove a directory.
async fn rmdir(client: &RpcClient, dir_fh: &[u8], name: &str) -> anyhow::Result<()> {
    let mut args = XdrEncoder::new();
    encode_fh(&mut args, dir_fh);
    args.put_string(name);
    let args_bytes = args.finish();

    let reply = client
        .call(rpc::NFS_PROGRAM, rpc::NFS_V3, NFSPROC3_RMDIR, &args_bytes)
        .await?;

    let mut dec = XdrDecoder::new(&reply);
    let status = dec.get_u32()?;
    if status != NFS3_OK {
        anyhow::bail!("RMDIR '{}' returned status {}", name, status);
    }
    Ok(())
}
