//! Per-caller POSIX permission enforcement (#91).
//!
//! The server process usually runs as root, so the filesystem itself checks
//! nothing on the caller's behalf. Every COMPOUND op that reads or changes an
//! object is checked here first, against the object's mode bits as stat()
//! reports them now, for the caller's identity after the export's `squash`
//! mapping. Objects the caller creates are chowned to that identity.
//!
//! What is checked (the same rules knfsd applies):
//!
//! | op | needs |
//! |---|---|
//! | LOOKUP, LOOKUPP | x on the directory |
//! | READDIR | r on the directory |
//! | CREATE, LINK, OPEN (new file) | w+x on the directory |
//! | REMOVE, RENAME | w+x on the directory (both for RENAME), sticky bit |
//! | OPEN (existing file) | r and/or w per share_access, x on the directory |
//! | READ / WRITE, ALLOCATE | r (or x) / w on the file |
//! | COPY | r on the source, w on the destination |
//! | SETATTR | owner for mode/ACL/times, root for owner, owner + member for group, w for size |
//!
//! READ, WRITE, OPEN and SETATTR size let the file's owner through whatever
//! the mode says (knfsd's owner override: a file created 0444 can still be
//! written through the open that created it). Root (uid 0 *after* squash)
//! passes everything. An object that cannot be stat()ed is not checked here:
//! the op itself then fails with its own error.
//!
//! Paths here are export paths ("/a/b"), stat()ed beneath the export root's
//! fd (`ExportRoot`, #123), never through a symlink.


use nextnfs_proto::nfs4_proto::{FileAttrValue, NfsArgOp, NfsStat4, OpenClaim4, OpenFlag4};
use nextnfs_proto::rpc_proto::OpaqueAuth;

use super::beneath::ExportRoot;
use super::export_manager::AccessControl;
use super::filemanager::{FileManagerHandle, Filehandle, RealMeta};
use super::nfs40::op_pseudo;
use super::request::NfsRequest;

const MAY_READ: u32 = 4;
const MAY_WRITE: u32 = 2;
const MAY_EXEC: u32 = 1;

const OPEN4_SHARE_ACCESS_READ: u32 = 0x0000_0001;
const OPEN4_SHARE_ACCESS_WRITE: u32 = 0x0000_0002;

/// Identity an op runs as: the RPC credential mapped through the export's
/// squash rules. AUTH_NONE (and any flavour that carries no uid) is the
/// export's anonymous identity, never root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub uid: u32,
    pub gid: u32,
    pub gids: Vec<u32>,
}

impl Caller {
    pub fn new(cred: Option<&OpaqueAuth>, ac: Option<&AccessControl>) -> Self {
        let (anon_uid, anon_gid) = match ac {
            Some(ac) => ac.anon_ids(),
            None => (super::export_manager::DEFAULT_ANON_ID, super::export_manager::DEFAULT_ANON_ID),
        };
        match cred {
            Some(OpaqueAuth::AuthUnix(auth)) => {
                let (uid, gid, gids) = match ac {
                    Some(ac) => ac.map_ids(auth.uid, auth.gid, &auth.gids),
                    // No export selected (pseudo-root): exports(5) default.
                    None => super::export_manager::AccessControl::default_map_ids(
                        auth.uid, auth.gid, &auth.gids,
                    ),
                };
                Caller { uid, gid, gids }
            }
            _ => Caller {
                uid: anon_uid,
                gid: anon_gid,
                gids: Vec::new(),
            },
        }
    }

    pub fn is_root(&self) -> bool {
        self.uid == 0
    }

    pub fn in_group(&self, gid: u32) -> bool {
        self.gid == gid || self.gids.contains(&gid)
    }

    /// Mode bits (rwx as 4/2/1) that apply to this caller for `meta`.
    pub fn mode_bits(&self, meta: &RealMeta) -> u32 {
        if self.uid == meta.uid {
            (meta.mode >> 6) & 7
        } else if self.in_group(meta.gid) {
            (meta.mode >> 3) & 7
        } else {
            meta.mode & 7
        }
    }

    /// Whether the caller has all of `want` (MAY_* bits) on `meta`.
    pub fn may(&self, meta: &RealMeta, want: u32) -> bool {
        if self.is_root() {
            // Root still needs some x bit to execute a non-directory.
            let is_dir = meta.mode & libc::S_IFMT == libc::S_IFDIR;
            return want & MAY_EXEC == 0 || is_dir || meta.mode & 0o111 != 0;
        }
        self.mode_bits(meta) & want == want
    }

    /// File access with knfsd's owner override (READ, WRITE, OPEN, size).
    fn may_file(&self, meta: &RealMeta, want: u32) -> bool {
        if !self.is_root() && self.uid == meta.uid {
            return true;
        }
        // Reading an executable-only file is allowed (clients read to exec).
        if want == MAY_READ && self.may(meta, MAY_EXEC) && meta.mode & libc::S_IFMT == libc::S_IFREG {
            return true;
        }
        self.may(meta, want)
    }

    fn is_owner(&self, meta: &RealMeta) -> bool {
        self.is_root() || self.uid == meta.uid
    }
}

fn stat(root: &ExportRoot, path: &str) -> Option<RealMeta> {
    root.symlink_metadata(path).ok().map(|m| RealMeta::from_metadata(&m))
}

/// `name` in the directory at export path `dir`.
fn join(dir: &str, name: &str) -> String {
    format!("{}/{}", dir.trim_end_matches('/'), name)
}

/// The parent of export path `path` ("/" for the root and its children).
fn parent(path: &str) -> &str {
    match path.trim_end_matches('/').rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

/// Directory an op on `fh` works in: `fh` itself, or its parent when `fh`
/// is not a directory (CREATE does the same).
fn dir_path<'a>(root: &ExportRoot, fh: &'a Filehandle) -> &'a str {
    match stat(root, &fh.path) {
        Some(m) if m.mode & libc::S_IFMT != libc::S_IFDIR => parent(&fh.path),
        _ => &fh.path,
    }
}

fn need(ok: bool, status: NfsStat4) -> Result<(), NfsStat4> {
    if ok { Ok(()) } else { Err(status) }
}

/// `want` on the directory at `dir`. Skipped when it cannot be stat()ed or
/// is not a directory: the op then fails with its own error (NOTDIR, ...).
fn need_dir(caller: &Caller, root: &ExportRoot, dir: &str, want: u32) -> Result<(), NfsStat4> {
    match stat(root, dir) {
        Some(m) if m.mode & libc::S_IFMT == libc::S_IFDIR => {
            need(caller.may(&m, want), NfsStat4::Nfs4errAccess)
        }
        _ => Ok(()),
    }
}

/// Removing or renaming `name` out of `dir`: w+x on `dir`, and in a sticky
/// directory only the owner of the directory or of the entry may do it.
fn need_unlink(caller: &Caller, root: &ExportRoot, dir: &str, name: &str) -> Result<(), NfsStat4> {
    let Some(dmeta) = stat(root, dir) else { return Ok(()) };
    if dmeta.mode & libc::S_IFMT != libc::S_IFDIR {
        return Ok(());
    }
    need(caller.may(&dmeta, MAY_WRITE | MAY_EXEC), NfsStat4::Nfs4errAccess)?;
    if dmeta.mode & libc::S_ISVTX != 0 && !caller.is_owner(&dmeta) {
        if let Some(entry) = stat(root, &join(dir, name)) {
            need(caller.is_owner(&entry), NfsStat4::Nfs4errPerm)?;
        }
    }
    Ok(())
}

fn need_file(caller: &Caller, root: &ExportRoot, path: &str, want: u32) -> Result<(), NfsStat4> {
    match stat(root, path) {
        Some(m) => need(caller.may_file(&m, want), NfsStat4::Nfs4errAccess),
        None => Ok(()),
    }
}

fn share_want(share_access: u32) -> u32 {
    let mut want = 0;
    if share_access & OPEN4_SHARE_ACCESS_READ != 0 {
        want |= MAY_READ;
    }
    if share_access & OPEN4_SHARE_ACCESS_WRITE != 0 {
        want |= MAY_WRITE;
    }
    want
}

fn check_setattr(
    caller: &Caller,
    root: &ExportRoot,
    path: &str,
    attrs: &[FileAttrValue],
) -> Result<(), NfsStat4> {
    let Some(meta) = stat(root, path) else { return Ok(()) };
    for attr in attrs {
        match attr {
            FileAttrValue::Size(_) => {
                need(caller.may_file(&meta, MAY_WRITE), NfsStat4::Nfs4errAccess)?
            }
            FileAttrValue::Mode(_) | FileAttrValue::Acl(_) => {
                need(caller.is_owner(&meta), NfsStat4::Nfs4errPerm)?
            }
            FileAttrValue::Owner(owner) => {
                // Only root gives a file away; the owner may "set" their own uid.
                let uid = FileManagerHandle::resolve_nfs4_uid(owner);
                let noop = uid == Some(meta.uid) && caller.uid == meta.uid;
                need(caller.is_root() || noop, NfsStat4::Nfs4errPerm)?
            }
            FileAttrValue::OwnerGroup(group) => {
                // The owner may move a file into a group they belong to.
                let ok = caller.is_root()
                    || (caller.uid == meta.uid
                        && FileManagerHandle::resolve_nfs4_gid(group)
                            .is_some_and(|g| caller.in_group(g)));
                need(ok, NfsStat4::Nfs4errPerm)?
            }
            // Set to server time: the owner, or anyone who may write.
            FileAttrValue::TimeAccessSet | FileAttrValue::TimeModifySet => need(
                caller.is_owner(&meta) || caller.may(&meta, MAY_WRITE),
                NfsStat4::Nfs4errAccess,
            )?,
            _ => {}
        }
    }
    Ok(())
}

fn is_symlink(root: &ExportRoot, path: &str) -> bool {
    stat(root, path).is_some_and(|m| m.mode & libc::S_IFMT == libc::S_IFLNK)
}

/// Whether reaching export path `path` passes through a symlink (or a
/// `.`/`..` component): every component before the last must be a real
/// directory, or the path can lead out of the export (#122). The lookup is
/// openat2 beneath the root, which fails ELOOP on a symlink on the way and
/// EXDEV on a way out. A component that does not exist is not a symlink;
/// the op then fails on its own (and, being resolved the same way, cannot
/// leave the export either, #123).
fn escapes_root(root: &ExportRoot, path: &str) -> bool {
    if path.split('/').any(|c| c == "." || c == "..") {
        return true;
    }
    match root.symlink_metadata(path) {
        Ok(_) => false,
        Err(e) => matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::EXDEV)),
    }
}

/// A component name a client may give (RFC 7530 §12.7): not empty
/// (NFS4ERR_INVAL), not `.` or `..` and without `/` or NUL
/// (NFS4ERR_BADNAME). A name with a `/` would be walked as several
/// components, through symlinks and past the search check of each (#122).
fn check_name(name: &str) -> Result<(), NfsStat4> {
    if name.is_empty() {
        return Err(NfsStat4::Nfs4errInval);
    }
    if name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(NfsStat4::Nfs4errBadname);
    }
    Ok(())
}

/// Keep `arg` inside its export (#122), whoever the caller is (root too):
///
/// - names are single components (`check_name`);
/// - the current (and, for RENAME, LINK and COPY, the saved) filehandle is
///   reached without passing through a symlink, else NFS4ERR_STALE: the
///   object the handle was given for is no longer at that path;
/// - an op that works in a directory refuses a symlink handle
///   (NFS4ERR_SYMLINK), and OPEN refuses a name that is a symlink
///   (NFS4ERR_SYMLINK: the client follows it itself, the server never does);
/// - READ, WRITE, COMMIT, ALLOCATE, SEEK and COPY refuse a symlink handle
///   (NFS4ERR_INVAL: not a regular file), and SETATTR refuses size, mode and
///   ACL on one (NFS4ERR_INVAL). Owner, group and times are set on the link
///   itself (`set_attr` never follows it).
pub fn check_confined(arg: &NfsArgOp, request: &NfsRequest<'_>) -> Result<(), NfsStat4> {
    if request.is_pseudo_root() {
        return Ok(());
    }
    let Some(fm) = request.file_manager_opt() else { return Ok(()) };
    let Some(fh) = request.current_filehandle() else { return Ok(()) };
    let root = fm.beneath();
    let current = fh.path.as_str();
    let saved = match arg {
        NfsArgOp::Oprename(_) | NfsArgOp::Oplink(_) | NfsArgOp::Opcopy(_) => {
            request.saved_filehandle().map(|s| s.path.as_str())
        }
        _ => None,
    };

    match arg {
        NfsArgOp::Oplookup(a) => check_name(&a.objname)?,
        NfsArgOp::Opcreate(a) => check_name(&a.objname)?,
        NfsArgOp::Opremove(a) => check_name(&a.target)?,
        NfsArgOp::Oprename(a) => {
            check_name(&a.oldname)?;
            check_name(&a.newname)?;
        }
        NfsArgOp::Oplink(a) => check_name(&a.newname)?,
        NfsArgOp::OpSecinfo(a) => check_name(&a.name)?,
        NfsArgOp::Opopen(a) => {
            if let OpenClaim4::ClaimNull(name) = &a.claim {
                check_name(name)?;
            }
        }
        _ => {}
    }

    if escapes_root(root, current) || saved.is_some_and(|s| escapes_root(root, s)) {
        return Err(NfsStat4::Nfs4errStale);
    }

    let need_not_symlink = |path: &str, status: NfsStat4| {
        if is_symlink(root, path) { Err(status) } else { Ok(()) }
    };
    match arg {
        NfsArgOp::Oplookup(_)
        | NfsArgOp::Opcreate(_)
        | NfsArgOp::Opremove(_)
        | NfsArgOp::Oplink(_)
        | NfsArgOp::Opreaddir(_)
        | NfsArgOp::OpSecinfo(_) => need_not_symlink(current, NfsStat4::Nfs4errSymlink),
        NfsArgOp::Oprename(_) => {
            need_not_symlink(current, NfsStat4::Nfs4errSymlink)?;
            match saved {
                Some(s) => need_not_symlink(s, NfsStat4::Nfs4errSymlink),
                None => Ok(()),
            }
        }
        NfsArgOp::Opopen(a) => match &a.claim {
            OpenClaim4::ClaimNull(name) => {
                need_not_symlink(current, NfsStat4::Nfs4errSymlink)?;
                need_not_symlink(&join(current, name), NfsStat4::Nfs4errSymlink)
            }
            _ => need_not_symlink(current, NfsStat4::Nfs4errSymlink),
        },
        NfsArgOp::Opread(_)
        | NfsArgOp::Opwrite(_)
        | NfsArgOp::Opcommit(_)
        | NfsArgOp::Opallocate(_)
        | NfsArgOp::Opseek(_) => need_not_symlink(current, NfsStat4::Nfs4errInval),
        NfsArgOp::Opcopy(_) => {
            need_not_symlink(current, NfsStat4::Nfs4errInval)?;
            match saved {
                Some(s) => need_not_symlink(s, NfsStat4::Nfs4errInval),
                None => Ok(()),
            }
        }
        NfsArgOp::Opsetattr(a) => {
            let content = a.obj_attributes.attr_vals.0.iter().any(|v| {
                matches!(v, FileAttrValue::Size(_) | FileAttrValue::Mode(_) | FileAttrValue::Acl(_))
            });
            if content {
                need_not_symlink(current, NfsStat4::Nfs4errInval)
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    }
}

/// Check `arg` for `caller` before it runs. `Err` is the status to refuse it
/// with: NFS4ERR_ACCESS for a mode-bit denial, NFS4ERR_PERM for an
/// owner-only change, NFS4ERR_XDEV for RENAME/LINK across exports, and the
/// export-confinement errors of `check_confined`.
pub fn check_op(arg: &NfsArgOp, request: &NfsRequest<'_>, caller: &Caller) -> Result<(), NfsStat4> {
    check_confined(arg, request)?;
    if request.is_pseudo_root() {
        return Ok(());
    }
    let Some(fm) = request.file_manager_opt() else { return Ok(()) };
    let Some(fh) = request.current_filehandle() else { return Ok(()) };
    let root = fm.beneath();
    let current = fh.path.as_str();

    match arg {
        NfsArgOp::Oplookup(_) | NfsArgOp::Oplookupp(_) => need_dir(caller, root, current, MAY_EXEC),
        NfsArgOp::Opreaddir(_) => need_dir(caller, root, current, MAY_READ),
        NfsArgOp::Opcreate(_) => need_dir(caller, root, dir_path(root, fh), MAY_WRITE | MAY_EXEC),
        NfsArgOp::Opremove(args) => need_unlink(caller, root, current, &args.target),
        NfsArgOp::Oprename(args) => {
            let Some(saved) = request.saved_filehandle() else { return Ok(()) };
            if op_pseudo::export_id_from_fh(&saved.id) != op_pseudo::export_id_from_fh(&fh.id) {
                return Err(NfsStat4::Nfs4errXdev);
            }
            need_unlink(caller, root, &saved.path, &args.oldname)?;
            need_unlink(caller, root, current, &args.newname)
        }
        NfsArgOp::Oplink(_) => {
            if let Some(saved) = request.saved_filehandle() {
                if op_pseudo::export_id_from_fh(&saved.id) != op_pseudo::export_id_from_fh(&fh.id) {
                    return Err(NfsStat4::Nfs4errXdev);
                }
            }
            need_dir(caller, root, current, MAY_WRITE | MAY_EXEC)
        }
        NfsArgOp::Opopen(args) => match &args.claim {
            OpenClaim4::ClaimNull(name) => {
                need_dir(caller, root, current, MAY_EXEC)?;
                let target = join(current, name);
                if stat(root, &target).is_some() {
                    need_file(caller, root, &target, share_want(args.share_access))
                } else if matches!(args.openhow, OpenFlag4::How(_)) {
                    need_dir(caller, root, current, MAY_WRITE | MAY_EXEC)
                } else {
                    Ok(())
                }
            }
            _ => need_file(caller, root, current, share_want(args.share_access)),
        },
        NfsArgOp::Opread(_) => need_file(caller, root, current, MAY_READ),
        NfsArgOp::Opwrite(_) | NfsArgOp::Opallocate(_) => need_file(caller, root, current, MAY_WRITE),
        NfsArgOp::Opcopy(_) => {
            if let Some(saved) = request.saved_filehandle() {
                need_file(caller, root, &saved.path, MAY_READ)?;
            }
            need_file(caller, root, current, MAY_WRITE)
        }
        NfsArgOp::Opsetattr(args) => {
            check_setattr(caller, root, current, &args.obj_attributes.attr_vals.0)
        }
        _ => Ok(()),
    }
}

/// Whether `arg` will create a new object (OPEN with a create mode on a
/// name that does not exist yet, or CREATE). Evaluated before the op runs.
pub fn creates_object(arg: &NfsArgOp, request: &NfsRequest<'_>) -> bool {
    match arg {
        NfsArgOp::Opcreate(_) => true,
        NfsArgOp::Opopen(args) => {
            let OpenClaim4::ClaimNull(name) = &args.claim else { return false };
            if !matches!(args.openhow, OpenFlag4::How(_)) {
                return false;
            }
            match (request.file_manager_opt(), request.current_filehandle()) {
                (Some(fm), Some(fh)) => stat(fm.beneath(), &join(&fh.path, name)).is_none(),
                _ => false,
            }
        }
        _ => false,
    }
}

/// Give the object just created (the current filehandle) to `caller`: uid is
/// the caller's, gid the caller's or, in a setgid directory, the directory's.
/// Fails quietly when the server may not chown (it does not run as root).
pub async fn chown_new_object(request: &mut NfsRequest<'_>, caller: &Caller) {
    let Some(fm) = request.file_manager_opt().cloned() else { return };
    let Some(fh) = request.current_filehandle().cloned() else { return };
    let root = fm.beneath();
    let Some(meta) = stat(root, &fh.path) else { return };
    let gid = match stat(root, parent(&fh.path)) {
        Some(parent) if parent.mode & libc::S_ISGID != 0 => parent.gid,
        _ => caller.gid,
    };
    if meta.uid == caller.uid && meta.gid == gid {
        return;
    }
    // fchownat on an O_PATH fd of the object itself, resolved beneath the
    // export root (#123): a new symlink is chowned itself, not its target.
    if let Err(e) = root.lchown(&fh.path, caller.uid, gid) {
        tracing::debug!(
            path = %fh.path,
            uid = caller.uid,
            gid,
            "chown of new object failed: {e}"
        );
        return;
    }
    let mut fh = fh;
    fh.attr_owner = caller.uid.to_string();
    fh.attr_owner_group = gid.to_string();
    fm.update_filehandle(fh.clone()).await;
    request.cache_filehandle(fh.clone());
    request.set_filehandle(fh);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::export_manager::{AccessConfig, SquashMode};

    #[test]
    fn test_export_path_helpers() {
        assert_eq!(join("/", "a"), "/a");
        assert_eq!(join("/a", "b"), "/a/b");
        assert_eq!(parent("/a"), "/");
        assert_eq!(parent("/a/b"), "/a");
        assert_eq!(parent("/"), "/");
    }

    #[test]
    fn test_escapes_root_by_descriptor() {
        let export = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(export.path().join("d")).unwrap();
        std::fs::write(export.path().join("d/f"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path(), export.path().join("l")).unwrap();
        let root = ExportRoot::open(export.path()).unwrap();
        assert!(!escapes_root(&root, "/d/f"));
        assert!(!escapes_root(&root, "/d/missing"));
        // The link itself is fine (an object), going through it is not.
        assert!(!escapes_root(&root, "/l"));
        assert!(escapes_root(&root, "/l/anything"));
        assert!(escapes_root(&root, "/d/../.."));
        assert!(is_symlink(&root, "/l"));
        assert!(!is_symlink(&root, "/d"));
    }
    use nextnfs_proto::rpc_proto::AuthUnix;

    fn unix(uid: u32, gid: u32, gids: Vec<u32>) -> OpaqueAuth {
        OpaqueAuth::AuthUnix(AuthUnix {
            stamp: 0,
            machinename: "c".to_string(),
            uid,
            gid,
            gids,
        })
    }

    fn meta(mode: u32, uid: u32, gid: u32) -> RealMeta {
        RealMeta {
            ino: 1, dev: 1, mode, nlink: 1, uid, gid, size: 0, blocks: 0,
            atime: 0, atime_nsec: 0, mtime: 0, mtime_nsec: 0, ctime: 0, ctime_nsec: 0,
        }
    }

    fn ac(squash: SquashMode) -> std::sync::Arc<AccessControl> {
        AccessControl::new(AccessConfig {
            squash,
            anon_uid: 99,
            anon_gid: 98,
            ..Default::default()
        })
    }

    #[test]
    fn test_caller_root_squash_maps_root() {
        let c = Caller::new(Some(&unix(0, 0, vec![0, 5])), Some(&ac(SquashMode::RootSquash)));
        assert_eq!(c, Caller { uid: 99, gid: 98, gids: vec![98, 5] });
        let c = Caller::new(Some(&unix(1000, 100, vec![100])), Some(&ac(SquashMode::RootSquash)));
        assert_eq!(c, Caller { uid: 1000, gid: 100, gids: vec![100] });
    }

    #[test]
    fn test_caller_all_squash_and_none() {
        let c = Caller::new(Some(&unix(1000, 100, vec![100])), Some(&ac(SquashMode::AllSquash)));
        assert_eq!(c, Caller { uid: 99, gid: 98, gids: vec![] });
        let c = Caller::new(Some(&unix(0, 0, vec![])), Some(&ac(SquashMode::None)));
        assert!(c.is_root());
    }

    #[test]
    fn test_caller_auth_none_is_anon() {
        let c = Caller::new(Some(&OpaqueAuth::AuthNull(vec![])), Some(&ac(SquashMode::None)));
        assert_eq!(c, Caller { uid: 99, gid: 98, gids: vec![] });
        let c = Caller::new(None, None);
        assert_eq!(c.uid, 65534);
    }

    #[test]
    fn test_caller_default_squash_is_root_squash() {
        let c = Caller::new(Some(&unix(0, 0, vec![])), None);
        assert_eq!(c.uid, 65534);
        let c = Caller::new(Some(&unix(0, 0, vec![])), Some(&AccessControl::new(AccessConfig::default())));
        assert_eq!((c.uid, c.gid), (65534, 65534));
    }

    #[test]
    fn test_may_owner_group_other() {
        let f = meta(libc::S_IFREG | 0o640, 1000, 100);
        let owner = Caller { uid: 1000, gid: 1, gids: vec![] };
        let member = Caller { uid: 2000, gid: 1, gids: vec![100] };
        let other = Caller { uid: 3000, gid: 1, gids: vec![] };
        assert!(owner.may(&f, MAY_READ | MAY_WRITE));
        assert!(member.may(&f, MAY_READ));
        assert!(!member.may(&f, MAY_WRITE));
        assert!(!other.may(&f, MAY_READ));
    }

    #[test]
    fn test_may_root_and_exec() {
        let root = Caller { uid: 0, gid: 0, gids: vec![] };
        assert!(root.may(&meta(libc::S_IFREG, 5, 5), MAY_READ | MAY_WRITE));
        assert!(!root.may(&meta(libc::S_IFREG | 0o644, 5, 5), MAY_EXEC));
        assert!(root.may(&meta(libc::S_IFDIR, 5, 5), MAY_EXEC));
    }

    #[test]
    fn test_owner_override_and_exec_read() {
        let owner = Caller { uid: 1000, gid: 1, gids: vec![] };
        assert!(owner.may_file(&meta(libc::S_IFREG | 0o444, 1000, 1), MAY_WRITE));
        let other = Caller { uid: 3000, gid: 1, gids: vec![] };
        assert!(other.may_file(&meta(libc::S_IFREG | 0o711, 1000, 2), MAY_READ));
        assert!(!other.may_file(&meta(libc::S_IFREG | 0o700, 1000, 2), MAY_READ));
    }

    #[test]
    fn test_setattr_rules() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        let root = ExportRoot::open(dir.path()).unwrap();
        let root = &root;
        let p = "/f";
        let m = stat(root, p).unwrap();
        let owner = Caller { uid: m.uid, gid: m.gid, gids: vec![] };
        let other = Caller { uid: m.uid.wrapping_add(4242), gid: 4242, gids: vec![] };

        assert!(check_setattr(&owner, root, p, &[FileAttrValue::Mode(0o600)]).is_ok());
        assert_eq!(check_setattr(&other, root, p, &[FileAttrValue::Mode(0o600)]), Err(NfsStat4::Nfs4errPerm));
        // Giving the file to someone else needs root.
        let give = FileAttrValue::Owner(other.uid.to_string());
        if !owner.is_root() {
            assert_eq!(check_setattr(&owner, root, p, std::slice::from_ref(&give)), Err(NfsStat4::Nfs4errPerm));
        }
        let keep = FileAttrValue::Owner(m.uid.to_string());
        assert!(check_setattr(&owner, root, p, &[keep]).is_ok());
        // chgrp to a group the owner is not in.
        if !owner.is_root() {
            let grp = FileAttrValue::OwnerGroup("4243".to_string());
            assert_eq!(check_setattr(&owner, root, p, &[grp]), Err(NfsStat4::Nfs4errPerm));
        }
        let grp = FileAttrValue::OwnerGroup(m.gid.to_string());
        assert!(check_setattr(&owner, root, p, &[grp]).is_ok());
    }

    #[test]
    fn test_sticky_unlink() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("f"), b"x").unwrap();
        let root = ExportRoot::open(d).unwrap();
        let root = &root;
        let m = stat(root, "/").unwrap();
        // Group/other may write, sticky set: a non-owner may not remove.
        std::fs::set_permissions(d, std::os::unix::fs::PermissionsExt::from_mode(0o1777)).unwrap();
        let other = Caller { uid: m.uid.wrapping_add(4242), gid: 4242, gids: vec![] };
        assert_eq!(need_unlink(&other, root, "/", "f"), Err(NfsStat4::Nfs4errPerm));
        std::fs::set_permissions(d, std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();
        assert!(need_unlink(&other, root, "/", "f").is_ok());
        std::fs::set_permissions(d, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(need_unlink(&other, root, "/", "f"), Err(NfsStat4::Nfs4errAccess));
    }
}
