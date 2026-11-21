//! Creation attributes of OPEN (UNCHECKED4, GUARDED4) and CREATE (#127,
//! review finding 0d36258c216e).
//!
//! A new object gets the attributes the client asked for, and gets its
//! permission bits from the moment it exists: it is created with the
//! requested bits (`O_CREAT|O_EXCL` or `mkdirat` with that mode, so the
//! server's umask can only narrow them), then chowned to the caller (or to
//! the owner/group the client asked for), then given the exact requested
//! mode (after the chown, which would clear set-id bits), then its size.
//! A private 0600 file is never 0644 on the way.
//!
//! Applied: `size`, `mode`, `owner`, `owner_group`. Any other writable
//! attribute is refused with NFS4ERR_ATTRNOTSUPP (nothing is created), a
//! read-only one with NFS4ERR_INVAL, an owner or group string that names
//! no one with NFS4ERR_BADOWNER, and giving the object to someone else (or
//! to a group the caller is not in) without being root with NFS4ERR_PERM.
//! The reply's `attrset` lists what was applied.

use std::io;
use std::os::unix::fs::MetadataExt;

use nextnfs_proto::nfs4_proto::{Attrlist4, Fattr4, FileAttr, FileAttrValue, NfsStat4};
use tracing::debug;

use super::filemanager::FileManagerHandle;
use super::perm::Caller;
use super::request::NfsRequest;

/// Mode a new regular file / directory gets when the client names none
/// (narrowed by the server's umask, as before #127).
pub const DEFAULT_FILE_MODE: u32 = 0o666;
pub const DEFAULT_DIR_MODE: u32 = 0o777;

/// The creation attributes nextnfs applies, parsed and validated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreateAttrs {
    pub size: Option<u64>,
    /// Permission bits, set-id and sticky bits included (`& 0o7777`).
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

/// Writable attributes nextnfs does not apply at creation.
fn writable_unsupported(attr: &FileAttr) -> bool {
    matches!(
        attr,
        FileAttr::Acl
            | FileAttr::Archive
            | FileAttr::Hidden
            | FileAttr::Mimetype
            | FileAttr::System
            | FileAttr::TimeAccessSet
            | FileAttr::TimeBackup
            | FileAttr::TimeCreate
            | FileAttr::TimeModifySet
    )
}

impl CreateAttrs {
    /// Parse `createattrs`. Every attribute in the mask must be one this
    /// server applies and must have been decoded.
    pub fn parse(f: &Fattr4) -> Result<Self, NfsStat4> {
        for attr in f.attrmask.iter() {
            match attr {
                FileAttr::Size | FileAttr::Mode | FileAttr::Owner | FileAttr::OwnerGroup => {}
                a if writable_unsupported(a) => {
                    debug!("createattrs: {:?} not supported", a);
                    return Err(NfsStat4::Nfs4errAttrnotsupp);
                }
                a => {
                    debug!("createattrs: {:?} is read-only", a);
                    return Err(NfsStat4::Nfs4errInval);
                }
            }
        }
        // The decoder stops at the first value it cannot read: a short
        // list is a malformed fattr4, never a partial set to apply.
        if f.attr_vals.len() != f.attrmask.len() {
            return Err(NfsStat4::Nfs4errBadxdr);
        }
        let mut out = CreateAttrs::default();
        for val in f.attr_vals.iter() {
            match val {
                FileAttrValue::Size(s) => out.size = Some(*s),
                FileAttrValue::Mode(m) => {
                    if *m & !0o7777 != 0 {
                        return Err(NfsStat4::Nfs4errInval);
                    }
                    out.mode = Some(*m);
                }
                FileAttrValue::Owner(s) => {
                    out.uid = Some(FileManagerHandle::resolve_nfs4_uid(s).ok_or(NfsStat4::Nfs4errBadOwner)?)
                }
                FileAttrValue::OwnerGroup(s) => {
                    out.gid = Some(FileManagerHandle::resolve_nfs4_gid(s).ok_or(NfsStat4::Nfs4errBadOwner)?)
                }
                // The mask check above admits only the four.
                _ => return Err(NfsStat4::Nfs4errBadxdr),
            }
        }
        Ok(out)
    }

    /// Whether `caller` may create an object with these attributes: only
    /// root gives an object to another uid or to a group it is not in.
    pub fn check(&self, caller: &Caller) -> Result<(), NfsStat4> {
        if caller.is_root() {
            return Ok(());
        }
        if self.uid.is_some_and(|u| u != caller.uid) {
            return Err(NfsStat4::Nfs4errPerm);
        }
        if self.gid.is_some_and(|g| !caller.in_group(g)) {
            return Err(NfsStat4::Nfs4errPerm);
        }
        Ok(())
    }

    /// The mode to create the object with: the requested permission bits
    /// (no set-id or sticky bits until it is owned right), or `default`.
    pub fn initial_mode(&self, default: u32) -> u32 {
        self.mode.map(|m| m & 0o777).unwrap_or(default)
    }
}

/// NFS status for an error creating an object on disk.
pub fn io_status(e: &io::Error) -> NfsStat4 {
    match e.raw_os_error() {
        Some(libc::EEXIST) => NfsStat4::Nfs4errExist,
        Some(libc::EACCES) => NfsStat4::Nfs4errAccess,
        Some(libc::EPERM) => NfsStat4::Nfs4errPerm,
        Some(libc::ENOENT) => NfsStat4::Nfs4errNoent,
        Some(libc::ENOTDIR) => NfsStat4::Nfs4errNotdir,
        Some(libc::EISDIR) => NfsStat4::Nfs4errIsdir,
        Some(libc::ELOOP) => NfsStat4::Nfs4errSymlink,
        Some(libc::ENOSPC) => NfsStat4::Nfs4errNospc,
        Some(libc::EDQUOT) => NfsStat4::Nfs4errDquot,
        Some(libc::EROFS) => NfsStat4::Nfs4errRofs,
        Some(libc::ENAMETOOLONG) => NfsStat4::Nfs4errNametoolong,
        Some(libc::EXDEV) => NfsStat4::Nfs4errXdev,
        Some(libc::EFBIG) => NfsStat4::Nfs4errFbig,
        _ => NfsStat4::Nfs4errIo,
    }
}

/// The parent of export path `path` ("/" for the root's children).
fn parent(path: &str) -> &str {
    match path.trim_end_matches('/').rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

/// Finish the object just created (the current filehandle): owner and
/// group (the requested ones, else the caller's; the group of a setgid
/// parent directory), then the exact requested mode, then the size. Returns
/// the attributes applied, for the reply's `attrset`. A chown the server
/// may not make (it does not run as root) leaves the object the server's,
/// with the requested mode; the owner is then not in `attrset`.
///
/// Nothing is applied on a vfs-only file manager (MemoryFS unit tests: no
/// file on disk), and the attrset is empty.
pub async fn finish_new_object(
    request: &mut NfsRequest<'_>,
    caller: &Caller,
    attrs: &CreateAttrs,
) -> Attrlist4<FileAttr> {
    let mut set = Attrlist4::<FileAttr>::new(None);
    let Some(fm) = request.file_manager_opt().cloned() else { return set };
    if fm.vfs_only() {
        return set;
    }
    let Some(fh) = request.current_filehandle().cloned() else { return set };
    let root = fm.beneath();
    let path = fh.path.clone();
    let Ok(made) = root.symlink_metadata(&path) else { return set };
    let kind = made.mode() & libc::S_IFMT;

    let gid = attrs.gid.unwrap_or_else(|| match root.symlink_metadata(parent(&path)) {
        Ok(p) if p.mode() & libc::S_ISGID != 0 => p.gid(),
        _ => caller.gid,
    });
    let uid = attrs.uid.unwrap_or(caller.uid);
    if (made.uid(), made.gid()) != (uid, gid) {
        // fchownat on the object itself, beneath the export root (#123):
        // a new symlink is chowned itself, not its target.
        if let Err(e) = root.lchown(&path, uid, gid) {
            debug!(path = %path, uid, gid, "chown of new object failed: {e}");
        }
    }

    let mut mode_set = false;
    if let Some(mode) = attrs.mode {
        // A symlink's mode is not settable (always 0777 on Linux).
        if kind != libc::S_IFLNK {
            let mut mode = mode;
            // A non-root caller's file in a group it is not in loses setgid,
            // as chmod(2) does.
            if kind == libc::S_IFREG && !caller.is_root() {
                let group = root.symlink_metadata(&path).map(|m| m.gid()).unwrap_or(gid);
                if !caller.in_group(group) {
                    mode &= !libc::S_ISGID;
                }
            }
            match root.chmod(&path, mode) {
                Ok(()) => mode_set = true,
                Err(e) => tracing::error!(path = %path, "chmod({mode:#o}) of new object failed: {e}"),
            }
        }
    }

    let mut size_set = false;
    if let Some(size) = attrs.size {
        match root.set_len(&path, size) {
            Ok(()) => size_set = true,
            Err(e) => tracing::error!(path = %path, "size {size} of new object failed: {e}"),
        }
    }

    if let Ok(now) = root.symlink_metadata(&path) {
        if size_set {
            set.push(FileAttr::Size);
        }
        if mode_set {
            set.push(FileAttr::Mode);
        }
        if attrs.uid == Some(now.uid()) {
            set.push(FileAttr::Owner);
        }
        if attrs.gid == Some(now.gid()) {
            set.push(FileAttr::OwnerGroup);
        }
    }
    refresh_current(request).await;
    set
}

/// Re-read size, mode, owner, group and change of the current filehandle's
/// object from disk into the filehandle (kept with its locks: an OPEN's
/// OPEN_CONFIRM finds them on it) and the caches.
pub async fn refresh_current(request: &mut NfsRequest<'_>) {
    let Some(fm) = request.file_manager_opt().cloned() else { return };
    let Some(mut fh) = request.current_filehandle().cloned() else { return };
    let Ok(now) = fm.beneath().symlink_metadata(&fh.path) else { return };
    fh.attr_size = now.size();
    fh.attr_mode = now.mode() & 0o7777;
    fh.attr_owner = now.uid().to_string();
    fh.attr_owner_group = now.gid().to_string();
    fh.attr_change = (now.mtime() as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(now.mtime_nsec() as u64);
    fm.update_filehandle(fh.clone()).await;
    request.cache_filehandle(fh.clone());
    request.set_filehandle(fh);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fattr(vals: Vec<FileAttrValue>) -> Fattr4 {
        let mask = vals
            .iter()
            .map(|v| match v {
                FileAttrValue::Size(_) => FileAttr::Size,
                FileAttrValue::Mode(_) => FileAttr::Mode,
                FileAttrValue::Owner(_) => FileAttr::Owner,
                FileAttrValue::OwnerGroup(_) => FileAttr::OwnerGroup,
                FileAttrValue::Acl(_) => FileAttr::Acl,
                FileAttrValue::Type(_) => FileAttr::Type,
                other => panic!("no mask bit in the test for {other:?}"),
            })
            .collect();
        Fattr4 { attrmask: Attrlist4(mask), attr_vals: Attrlist4(vals) }
    }

    fn caller(uid: u32, gid: u32, gids: Vec<u32>) -> Caller {
        Caller { uid, gid, gids }
    }

    #[test]
    fn test_parse_supported() {
        let a = CreateAttrs::parse(&fattr(vec![
            FileAttrValue::Size(0),
            FileAttrValue::Mode(0o4600),
            FileAttrValue::Owner("1000".into()),
            FileAttrValue::OwnerGroup("100".into()),
        ]))
        .unwrap();
        assert_eq!(
            a,
            CreateAttrs { size: Some(0), mode: Some(0o4600), uid: Some(1000), gid: Some(100) }
        );
        assert_eq!(a.initial_mode(0o666), 0o600);
        assert_eq!(CreateAttrs::default().initial_mode(0o666), 0o666);
    }

    #[test]
    fn test_parse_refuses_unsupported_and_readonly() {
        assert_eq!(
            CreateAttrs::parse(&fattr(vec![FileAttrValue::Acl(vec![])])),
            Err(NfsStat4::Nfs4errAttrnotsupp)
        );
        assert_eq!(
            CreateAttrs::parse(&fattr(vec![FileAttrValue::Type(
                nextnfs_proto::nfs4_proto::NfsFtype4::Nf4reg
            )])),
            Err(NfsStat4::Nfs4errInval)
        );
        // time_modify_set is not decoded: refused by its mask bit, not dropped.
        let f = Fattr4 {
            attrmask: Attrlist4(vec![FileAttr::Mode, FileAttr::TimeModifySet]),
            attr_vals: Attrlist4(vec![FileAttrValue::Mode(0o600)]),
        };
        assert_eq!(CreateAttrs::parse(&f), Err(NfsStat4::Nfs4errAttrnotsupp));
        // A value the decoder could not read: malformed, nothing applied.
        let f = Fattr4 {
            attrmask: Attrlist4(vec![FileAttr::Mode, FileAttr::Owner]),
            attr_vals: Attrlist4(vec![FileAttrValue::Mode(0o600)]),
        };
        assert_eq!(CreateAttrs::parse(&f), Err(NfsStat4::Nfs4errBadxdr));
        assert_eq!(
            CreateAttrs::parse(&fattr(vec![FileAttrValue::Mode(0o100644)])),
            Err(NfsStat4::Nfs4errInval)
        );
        assert_eq!(
            CreateAttrs::parse(&fattr(vec![FileAttrValue::Owner("no-such-user-127".into())])),
            Err(NfsStat4::Nfs4errBadOwner)
        );
    }

    #[test]
    fn test_check_giving_away() {
        let own = CreateAttrs { uid: Some(1000), gid: Some(100), ..Default::default() };
        assert_eq!(own.check(&caller(1000, 1000, vec![100])), Ok(()));
        assert_eq!(own.check(&caller(1000, 1000, vec![])), Err(NfsStat4::Nfs4errPerm));
        assert_eq!(own.check(&caller(2000, 100, vec![])), Err(NfsStat4::Nfs4errPerm));
        assert_eq!(own.check(&caller(0, 0, vec![])), Ok(()));
    }

    #[test]
    fn test_io_status() {
        assert_eq!(io_status(&io::Error::from_raw_os_error(libc::EEXIST)), NfsStat4::Nfs4errExist);
        assert_eq!(io_status(&io::Error::from_raw_os_error(libc::ELOOP)), NfsStat4::Nfs4errSymlink);
        assert_eq!(io_status(&io::Error::from_raw_os_error(libc::EACCES)), NfsStat4::Nfs4errAccess);
    }
}
