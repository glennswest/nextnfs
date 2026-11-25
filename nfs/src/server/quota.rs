//! Per-export quotas on the filesystem's own project quotas (#146).
//!
//! An export with a quota is one project on its filesystem (XFS mounted with
//! `prjquota`, or ext4 with the `project` and `quota` features mounted with
//! `prjquota`). nextnfs gives the export directory and everything in it a
//! project id with the inherit flag (`FS_IOC_FSSETXATTR`, what `xfs_quota -x
//! -c 'project -s'` does), and sets the project's block limits
//! (`quotactl_fd(Q_SETQUOTA, PRJQUOTA)`). From then on the kernel does the
//! accounting and the enforcement: every allocation in the tree, whether it
//! comes from NFS or from a local process, is charged to the project when it
//! happens and released when the blocks are freed, and a write past the hard
//! limit fails with EDQUOT (NFS4ERR_DQUOT). New objects inherit the id from
//! their directory, and the kernel refuses to rename or link an object into
//! the tree from another project (EXDEV), so the count cannot drift.
//!
//! nextnfs only reads the usage back (`Q_GETQUOTA`) for GETATTR's `quota_*`
//! attributes, the REST API and the web UI.
//!
//! What it needs: Linux >= 5.14 (`quotactl_fd`), project quota accounting
//! and enforcement on, and CAP_SYS_ADMIN (to set limits and read a
//! project's usage) plus a process in the initial user namespace (to change
//! project ids). An export configured with a quota where any of this is
//! missing is refused with the reason; a quota is never silently ignored.

use std::collections::HashSet;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::rc::Rc;

use serde::{Deserialize, Serialize};

use nextnfs_proto::nfs4_proto::{FileAttr, FileAttrValue};

use super::beneath::ExportRoot;

/// `quotactl_fd(2)`, Linux 5.14; the same number on every architecture.
const SYS_QUOTACTL_FD: libc::c_long = 443;

const PRJQUOTA: u32 = 2;
const Q_GETQUOTA: u32 = 0x800007;
const Q_SETQUOTA: u32 = 0x800008;
/// `Q_XGETQSTAT` (`XQM_CMD(5)`): quota state, allowed without privilege.
const Q_XGETQSTAT: u32 = ((b'X' as u32) << 8) | 5;
const FS_QUOTA_PDQ_ACCT: u16 = 1 << 4;
const FS_QUOTA_PDQ_ENFD: u16 = 1 << 5;
/// `if_dqblk.dqb_valid`: the block limits are set.
const QIF_BLIMITS: u32 = 1;
/// Quota block limits are in units of 1 KiB (`QIF_DQBLKSIZE`).
const QUOTA_BLOCK: u64 = 1024;

/// `struct fsxattr` (linux/fs.h).
#[repr(C)]
#[allow(dead_code)] // filled in by the kernel; not every field is read
#[derive(Debug, Default, Clone, Copy)]
struct FsXattr {
    xflags: u32,
    extsize: u32,
    nextents: u32,
    projid: u32,
    cowextsize: u32,
    pad: [u8; 8],
}

/// `_IOR('X', 31, struct fsxattr)` / `_IOW('X', 32, struct fsxattr)`.
const FS_IOC_FSGETXATTR: libc::c_ulong = 0x801c_581f;
const FS_IOC_FSSETXATTR: libc::c_ulong = 0x401c_5820;
const FS_XFLAG_PROJINHERIT: u32 = 0x0000_0200;

/// `struct if_dqblk` (linux/quota.h).
#[repr(C)]
#[allow(dead_code)] // filled in by the kernel; not every field is read
#[derive(Debug, Default, Clone, Copy)]
struct IfDqblk {
    bhardlimit: u64,
    bsoftlimit: u64,
    curspace: u64,
    ihardlimit: u64,
    isoftlimit: u64,
    curinodes: u64,
    btime: u64,
    itime: u64,
    valid: u32,
}

fn qcmd(cmd: u32, kind: u32) -> u32 {
    (cmd << 8) | (kind & 0xff)
}

fn quotactl_fd(fd: RawFd, cmd: u32, id: u32, addr: *mut libc::c_void) -> io::Result<()> {
    let ret = unsafe { libc::syscall(SYS_QUOTACTL_FD, fd as libc::c_uint, cmd, id, addr) };
    if ret == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

fn get_fsxattr(fd: RawFd) -> io::Result<FsXattr> {
    let mut fa = FsXattr::default();
    let ret = unsafe { libc::ioctl(fd, FS_IOC_FSGETXATTR as _, &mut fa as *mut FsXattr) };
    if ret == 0 { Ok(fa) } else { Err(io::Error::last_os_error()) }
}

/// Give the object open on `fd` project `id` (and the inherit flag when it
/// is a directory), keeping its other attributes.
fn set_project(fd: RawFd, id: u32, dir: bool) -> io::Result<()> {
    let mut fa = get_fsxattr(fd)?;
    let want = if dir { fa.xflags | FS_XFLAG_PROJINHERIT } else { fa.xflags };
    if fa.projid == id && fa.xflags == want {
        return Ok(());
    }
    fa.projid = id;
    fa.xflags = want;
    let ret = unsafe { libc::ioctl(fd, FS_IOC_FSSETXATTR as _, &fa as *const FsXattr) };
    if ret == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Project quota state of the filesystem of `fd`: (accounting, enforcing).
fn project_quota_state(fd: RawFd) -> io::Result<(bool, bool)> {
    // struct fs_quota_stat: s8 version, then u16 qs_flags at offset 2. The
    // buffer is larger than the struct (about 80 bytes) and 8-aligned.
    let mut buf = [0u64; 32];
    quotactl_fd(fd, qcmd(Q_XGETQSTAT, PRJQUOTA), 0, buf.as_mut_ptr().cast())?;
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), 4) };
    let flags = u16::from_ne_bytes([bytes[2], bytes[3]]);
    Ok((flags & FS_QUOTA_PDQ_ACCT != 0, flags & FS_QUOTA_PDQ_ENFD != 0))
}

fn get_dqblk(fd: RawFd, id: u32) -> io::Result<IfDqblk> {
    let mut dq = IfDqblk::default();
    quotactl_fd(fd, qcmd(Q_GETQUOTA, PRJQUOTA), id, (&mut dq as *mut IfDqblk).cast())?;
    Ok(dq)
}

/// A byte limit as quota blocks (1 KiB, rounded up: a limit is never
/// lower than asked).
fn to_blocks(bytes: u64) -> u64 {
    bytes.div_ceil(QUOTA_BLOCK)
}

/// What the configuration (`[[exports]] quota_*`) or the API asks for.
/// `None` leaves the filesystem's current value; `Some(0)` removes that
/// limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaLimits {
    /// Writes past this many bytes stored fail with NFS4ERR_DQUOT.
    #[serde(default)]
    pub hard_bytes: Option<u64>,
    /// The soft limit (the filesystem's grace period applies past it).
    #[serde(default)]
    pub soft_bytes: Option<u64>,
    /// The project id to use when the export directory has none. Unset:
    /// the directory's inode number (when it fits in 32 bits and no other
    /// project uses that id).
    #[serde(default)]
    pub project_id: Option<u32>,
}

impl QuotaLimits {
    /// Whether anything is asked for.
    pub fn is_set(&self) -> bool {
        self.hard_bytes.is_some() || self.soft_bytes.is_some() || self.project_id.is_some()
    }

    /// The limits in themselves: a soft limit above the hard one is refused.
    pub fn validate(&self) -> Result<(), String> {
        if let (Some(h), Some(s)) = (self.hard_bytes, self.soft_bytes) {
            if h != 0 && s > h {
                return Err(format!("quota soft limit {s} is above the hard limit {h}"));
            }
        }
        if self.project_id == Some(0) {
            return Err("quota project id 0 is the default project (every file outside one); pick another".into());
        }
        Ok(())
    }
}

/// An export's project quota as the filesystem reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct QuotaUsage {
    pub project_id: u32,
    /// Bytes stored in the project.
    pub used_bytes: u64,
    /// Hard limit in bytes, 0 = none.
    pub hard_bytes: u64,
    /// Soft limit in bytes, 0 = none.
    pub soft_bytes: u64,
    /// Inodes in the project.
    pub used_files: u64,
}

impl QuotaUsage {
    /// GETATTR `quota_avail_hard`: bytes that can still be stored before
    /// the hard limit, never more than the filesystem has (`fs_avail`).
    pub fn avail_hard(&self, fs_avail: u64) -> u64 {
        if self.hard_bytes == 0 {
            fs_avail
        } else {
            self.hard_bytes.saturating_sub(self.used_bytes).min(fs_avail)
        }
    }

    /// GETATTR `quota_avail_soft`: the same against the soft limit (the
    /// hard one when there is no soft limit).
    pub fn avail_soft(&self, fs_avail: u64) -> u64 {
        if self.soft_bytes == 0 {
            self.avail_hard(fs_avail)
        } else {
            self.soft_bytes.saturating_sub(self.used_bytes).min(fs_avail)
        }
    }
}

/// The GETATTR quota attributes (RFC 7530 §5.8.2.31–33). They are in
/// `supported_attrs` only for an export with a project quota.
pub const QUOTA_ATTRS: [FileAttr; 3] = [FileAttr::QuotaAvailHard, FileAttr::QuotaAvailSoft, FileAttr::QuotaUsed];

/// Whether a GETATTR asks for any quota attribute.
pub fn wants_quota(attrs: &[FileAttr]) -> bool {
    attrs.iter().any(|a| QUOTA_ATTRS.contains(a))
}

/// `supported_attrs` plus the quota attributes when `root` has a quota.
pub fn add_supported(root: &ExportRoot, attrs: &mut Vec<FileAttr>) {
    if root.quota_project().is_some() {
        attrs.extend_from_slice(&QUOTA_ATTRS);
    }
}

/// The value of quota attribute `attr` for an export whose usage is
/// `quota` and whose filesystem has `fs_avail` bytes available; `None`
/// (left out of the reply) when there is no quota or `attr` is another one.
pub fn attr_value(attr: &FileAttr, quota: Option<&QuotaUsage>, fs_avail: u64) -> Option<FileAttrValue> {
    let q = quota?;
    match attr {
        FileAttr::QuotaAvailHard => Some(FileAttrValue::QuotaAvailHard(q.avail_hard(fs_avail))),
        FileAttr::QuotaAvailSoft => Some(FileAttrValue::QuotaAvailSoft(q.avail_soft(fs_avail))),
        FileAttr::QuotaUsed => Some(FileAttrValue::QuotaUsed(q.used_bytes)),
        _ => None,
    }
}

/// For GETATTR: the usage of `root` when `attrs` asks for it and there is
/// a quota (a failed read is logged and leaves the attributes out).
pub fn usage_for(root: &ExportRoot, attrs: &[FileAttr]) -> Option<QuotaUsage> {
    if !wants_quota(attrs) {
        return None;
    }
    match usage(root) {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(path = %root.path().display(), "reading the export's project quota: {e}");
            None
        }
    }
}

/// The project quota of `root`, when it has one: the project id given by
/// `setup` or found by `probe`, read from the filesystem now.
pub fn usage(root: &ExportRoot) -> io::Result<Option<QuotaUsage>> {
    let Some(id) = root.quota_project() else { return Ok(None) };
    let fd = root.open_root_dir()?;
    let dq = get_dqblk(fd.as_raw_fd(), id)?;
    Ok(Some(QuotaUsage {
        project_id: id,
        used_bytes: dq.curspace,
        hard_bytes: dq.bhardlimit.saturating_mul(QUOTA_BLOCK),
        soft_bytes: dq.bsoftlimit.saturating_mul(QUOTA_BLOCK),
        used_files: dq.curinodes,
    }))
}

/// For an export added without a quota in its configuration: if its
/// directory already is an enforced project with a limit (set up by an
/// administrator or an earlier run), report that project. Quiet otherwise.
pub fn probe(root: &ExportRoot) {
    let Ok(fd) = root.open_root_dir() else { return };
    let fd = fd.as_raw_fd();
    let Ok(fa) = get_fsxattr(fd) else { return };
    if fa.projid == 0 || fa.xflags & FS_XFLAG_PROJINHERIT == 0 {
        return;
    }
    if !matches!(project_quota_state(fd), Ok((true, true))) {
        return;
    }
    match get_dqblk(fd, fa.projid) {
        Ok(dq) if dq.bhardlimit != 0 || dq.bsoftlimit != 0 => {
            tracing::info!(path = %root.path().display(), project = fa.projid, "export directory has a project quota; reporting it");
            root.set_quota_project(Some(fa.projid));
        }
        _ => {}
    }
}

/// Make `root` a project with `limits` (see the module doc). Returns the
/// project id. Refused, with the reason, when the filesystem or the
/// process cannot do it.
pub fn setup(root: &ExportRoot, limits: &QuotaLimits) -> Result<u32, String> {
    limits.validate()?;
    let path = root.path().display().to_string();
    let dir = root.open_root_dir().map_err(|e| format!("{path}: {e}"))?;
    let fd = dir.as_raw_fd();

    match project_quota_state(fd) {
        Ok((true, true)) => {}
        Ok((true, false)) => {
            return Err(format!(
                "{path}: project quotas are counted but not enforced on its filesystem (mounted with pqnoenforce?); mount it with prjquota"
            ))
        }
        Ok((false, _)) => {
            return Err(format!(
                "{path}: its filesystem has no project quotas: mount XFS with prjquota, or ext4 made with the project and quota features with prjquota"
            ))
        }
        Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {
            return Err(format!(
                "{path}: quotactl_fd(2) answered ENOSYS: its filesystem has no project quota support, or the kernel has no quotactl_fd (Linux >= 5.14)"
            ))
        }
        Err(e) => {
            return Err(format!("{path}: its filesystem has no project quotas ({e})"))
        }
    }

    let fa = get_fsxattr(fd).map_err(|e| format!("{path}: reading its project id: {e}"))?;
    let id = if fa.projid != 0 {
        if let Some(want) = limits.project_id {
            if want != fa.projid {
                return Err(format!(
                    "{path} is already project {}; quota_project_id {want} would move it (unset the key, or change the directory's project first)",
                    fa.projid
                ));
            }
        }
        fa.projid
    } else {
        match limits.project_id {
            Some(id) => id,
            None => {
                let mut st: libc::stat = unsafe { std::mem::zeroed() };
                if unsafe { libc::fstat(fd, &mut st) } != 0 {
                    return Err(format!("{path}: {}", io::Error::last_os_error()));
                }
                let ino = st.st_ino as u64;
                if ino == 0 || ino > u64::from(u32::MAX) {
                    return Err(format!(
                        "{path}: its inode number {ino} cannot be a project id; set quota_project_id"
                    ));
                }
                // A derived id must be one nobody uses yet.
                let dq = get_dqblk(fd, ino as u32).map_err(|e| quota_err(&path, e))?;
                if dq.curspace != 0 || dq.curinodes != 0 || dq.bhardlimit != 0 || dq.bsoftlimit != 0
                    || dq.ihardlimit != 0 || dq.isoftlimit != 0
                {
                    return Err(format!(
                        "{path}: project id {ino} (its inode number) is already in use on the filesystem; set quota_project_id"
                    ));
                }
                ino as u32
            }
        }
    };

    // The tree first, the root last: a root carrying the id and the inherit
    // flag means a walk finished (a later start does not walk again).
    if fa.projid != id || fa.xflags & FS_XFLAG_PROJINHERIT == 0 {
        let tagged = tag_tree(&dir, id).map_err(|e| format!("{path}: setting project {id} on its files: {e}"))?;
        set_project(fd, id, true).map_err(|e| format!("{path}: setting project {id}: {}", perm_hint(e)))?;
        tracing::info!(%path, project = id, objects = tagged, "export directory made a quota project");
    }

    // Limits: the ones asked for, the filesystem's current ones otherwise.
    let mut dq = get_dqblk(fd, id).map_err(|e| quota_err(&path, e))?;
    if limits.hard_bytes.is_some() || limits.soft_bytes.is_some() {
        if let Some(h) = limits.hard_bytes {
            dq.bhardlimit = to_blocks(h);
        }
        if let Some(s) = limits.soft_bytes {
            dq.bsoftlimit = to_blocks(s);
        }
        if dq.bhardlimit != 0 && dq.bsoftlimit > dq.bhardlimit {
            return Err(format!(
                "{path}: soft limit {} bytes would be above the hard limit {} bytes",
                dq.bsoftlimit * QUOTA_BLOCK,
                dq.bhardlimit * QUOTA_BLOCK
            ));
        }
        dq.valid = QIF_BLIMITS;
        quotactl_fd(fd, qcmd(Q_SETQUOTA, PRJQUOTA), id, (&mut dq as *mut IfDqblk).cast())
            .map_err(|e| quota_err(&path, e))?;
    }
    root.set_quota_project(Some(id));
    tracing::info!(
        %path,
        project = id,
        hard_bytes = dq.bhardlimit * QUOTA_BLOCK,
        soft_bytes = dq.bsoftlimit * QUOTA_BLOCK,
        "export quota set"
    );
    Ok(id)
}

fn perm_hint(e: io::Error) -> String {
    match e.raw_os_error() {
        Some(libc::EPERM) | Some(libc::EACCES) => {
            format!("{e} (setting project quotas needs CAP_SYS_ADMIN in the initial user namespace)")
        }
        _ => e.to_string(),
    }
}

fn quota_err(path: &str, e: io::Error) -> String {
    format!("{path}: project quota: {}", perm_hint(e))
}

/// Give every directory and regular file below the directory `top` project
/// `id` (directories with the inherit flag). Names are opened one component
/// at a time from their parent's fd with O_NOFOLLOW, so the walk never
/// leaves the tree through a symlink. Symlinks, devices, FIFOs and sockets
/// are left as they are (opening them is not safe or not possible); a
/// directory that already is another project (nonzero id) is left alone
/// with everything below it. Returns how many objects were changed.
fn tag_tree(top: &OwnedFd, id: u32) -> io::Result<u64> {
    let mut count = 0u64;
    let top = Rc::new(top.try_clone()?);
    let mut stack: Vec<(Rc<OwnedFd>, CString)> = Vec::new();
    // Hard links: a file is changed once.
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    push_children(&top, &mut stack)?;
    while let Some((parent, name)) = stack.pop() {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(parent.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ENOENT) {
                continue; // removed meanwhile
            }
            return Err(e);
        }
        let kind = st.st_mode & libc::S_IFMT;
        let is_dir = kind == libc::S_IFDIR;
        if !is_dir && kind != libc::S_IFREG {
            continue;
        }
        if !is_dir && !seen.insert((st.st_dev as u64, st.st_ino as u64)) {
            continue;
        }
        let kind_flag = if is_dir { libc::O_DIRECTORY } else { libc::O_NONBLOCK };
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NOCTTY | kind_flag;
        let raw = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if raw < 0 {
            let e = io::Error::last_os_error();
            // Removed or replaced (by a symlink, ...) meanwhile.
            if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ELOOP) | Some(libc::ENOTDIR)) {
                continue;
            }
            return Err(e);
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if is_dir {
            let fa = get_fsxattr(fd.as_raw_fd())?;
            if fa.projid != 0 && fa.projid != id {
                tracing::warn!(project = fa.projid, name = ?name, "directory in the export already is another project; left alone");
                continue;
            }
        }
        set_project(fd.as_raw_fd(), id, is_dir)?;
        count += 1;
        if is_dir {
            push_children(&Rc::new(fd), &mut stack)?;
        }
    }
    Ok(count)
}

/// Queue every entry of the directory `dir` (but `.` and `..`).
fn push_children(dir: &Rc<OwnedFd>, stack: &mut Vec<(Rc<OwnedFd>, CString)>) -> io::Result<()> {
    for entry in std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
        let entry = entry?;
        let name = CString::new(entry.file_name().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        stack.push((dir.clone(), name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(base: &Path, tag: &str) -> Self {
            let p = base.join(format!("nextnfs-quota-{}-{}", tag, std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Scratch(p)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn limits_validate() {
        assert!(QuotaLimits { hard_bytes: Some(10), soft_bytes: Some(20), project_id: None }.validate().is_err());
        assert!(QuotaLimits { hard_bytes: Some(0), soft_bytes: Some(20), project_id: None }.validate().is_ok());
        assert!(QuotaLimits { hard_bytes: Some(20), soft_bytes: Some(20), project_id: None }.validate().is_ok());
        assert!(QuotaLimits { project_id: Some(0), ..Default::default() }.validate().is_err());
        assert!(!QuotaLimits::default().is_set());
        assert!(QuotaLimits { hard_bytes: Some(0), ..Default::default() }.is_set());
    }

    #[test]
    fn byte_limits_round_up_to_quota_blocks() {
        assert_eq!(to_blocks(0), 0);
        assert_eq!(to_blocks(1), 1);
        assert_eq!(to_blocks(1024), 1);
        assert_eq!(to_blocks(1025), 2);
        assert_eq!(to_blocks(1 << 30), 1 << 20);
    }

    #[test]
    fn avail_is_limit_minus_used_capped_by_the_filesystem() {
        let u = QuotaUsage { project_id: 7, used_bytes: 300, hard_bytes: 1000, soft_bytes: 500, used_files: 2 };
        assert_eq!(u.avail_hard(u64::MAX), 700);
        assert_eq!(u.avail_soft(u64::MAX), 200);
        assert_eq!(u.avail_hard(100), 100);
        let over = QuotaUsage { used_bytes: 2000, ..u };
        assert_eq!(over.avail_hard(u64::MAX), 0);
        let none = QuotaUsage { hard_bytes: 0, soft_bytes: 0, ..u };
        assert_eq!(none.avail_hard(4242), 4242);
        assert_eq!(none.avail_soft(4242), 4242);
        let hard_only = QuotaUsage { soft_bytes: 0, ..u };
        assert_eq!(hard_only.avail_soft(u64::MAX), 700);
    }

    /// GETATTR: no quota → the attribute is left out; with one, the values.
    #[test]
    fn attr_values() {
        assert_eq!(attr_value(&FileAttr::QuotaUsed, None, 100), None);
        let u = QuotaUsage { project_id: 7, used_bytes: 300, hard_bytes: 1000, soft_bytes: 0, used_files: 2 };
        assert_eq!(attr_value(&FileAttr::QuotaUsed, Some(&u), 5000), Some(FileAttrValue::QuotaUsed(300)));
        assert_eq!(attr_value(&FileAttr::QuotaAvailHard, Some(&u), 5000), Some(FileAttrValue::QuotaAvailHard(700)));
        assert_eq!(attr_value(&FileAttr::QuotaAvailSoft, Some(&u), 500), Some(FileAttrValue::QuotaAvailSoft(500)));
        assert_eq!(attr_value(&FileAttr::Type, Some(&u), 500), None);
        assert!(wants_quota(&[FileAttr::Type, FileAttr::QuotaUsed]));
        assert!(!wants_quota(&[FileAttr::Type, FileAttr::SpaceAvail]));
    }

    /// The ioctl numbers are the kernel's (`_IOR`/`_IOW('X', 31/32,
    /// struct fsxattr)`): a wrong size in them would be ENOTTY everywhere.
    #[test]
    fn fsxattr_layout_matches_the_ioctl_numbers() {
        assert_eq!(std::mem::size_of::<FsXattr>(), 28);
        assert_eq!((FS_IOC_FSGETXATTR >> 16) & 0x3fff, 28);
        assert_eq!((FS_IOC_FSSETXATTR >> 16) & 0x3fff, 28);
        assert_eq!(std::mem::size_of::<IfDqblk>(), 72);
        assert_eq!(qcmd(Q_GETQUOTA, PRJQUOTA), 0x8000_0702);
    }

    /// On a filesystem without enforced project quotas (the build VM's
    /// scratch space), a quota is refused with the reason, and the export
    /// reports none. Never silently accepted.
    #[test]
    fn setup_refused_without_project_quotas() {
        let dir = Scratch::new(&std::env::temp_dir(), "noprj");
        let root = ExportRoot::open(&dir.0).unwrap();
        let fd = root.open_root_dir().unwrap();
        if matches!(project_quota_state(fd.as_raw_fd()), Ok((true, true))) {
            eprintln!("temp dir's filesystem enforces project quotas; refusal path not testable here");
            return;
        }
        let err = setup(&root, &QuotaLimits { hard_bytes: Some(1 << 20), ..Default::default() }).unwrap_err();
        assert!(err.contains("project quota") || err.contains("quotactl_fd"), "{err}");
        assert_eq!(root.quota_project(), None);
        assert_eq!(usage(&root).unwrap(), None);
        probe(&root);
        assert_eq!(root.quota_project(), None);
    }

    /// Needs root and a directory on a filesystem with enforced project
    /// quotas: `NEXTNFS_PRJQUOTA_DIR` (tests/quota.sh makes a loopback XFS
    /// with prjquota and sets it). Skipped (passes) without it.
    #[test]
    fn project_quota_enforced_and_reported() {
        let Some(base) = std::env::var_os("NEXTNFS_PRJQUOTA_DIR") else {
            eprintln!("NEXTNFS_PRJQUOTA_DIR not set; privileged project quota test skipped");
            return;
        };
        let dir = Scratch::new(Path::new(&base), "enforce");
        // Objects from before the quota are charged to it too.
        std::fs::create_dir_all(dir.0.join("old/sub")).unwrap();
        std::fs::write(dir.0.join("old/sub/f"), vec![1u8; 256 * 1024]).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.0.join("old/link")).unwrap();

        let root = ExportRoot::open(&dir.0).unwrap();
        let limits = QuotaLimits { hard_bytes: Some(1 << 20), soft_bytes: Some(512 * 1024), project_id: Some(4_146_001) };
        let id = setup(&root, &limits).unwrap();
        assert_eq!(id, 4_146_001);
        let u = usage(&root).unwrap().unwrap();
        assert_eq!(u.project_id, id);
        assert_eq!(u.hard_bytes, 1 << 20);
        assert_eq!(u.soft_bytes, 512 * 1024);
        assert!(u.used_bytes >= 256 * 1024, "pre-existing file not charged: {u:?}");

        // New files inherit the project; past the hard limit: EDQUOT.
        let f = root.open_file("/new", libc::O_WRONLY | libc::O_CREAT, 0o644).unwrap();
        let buf = vec![2u8; 64 * 1024];
        let mut err = None;
        for i in 0..64u64 {
            use std::os::unix::fs::FileExt;
            if let Err(e) = f.write_all_at(&buf, i * buf.len() as u64).and_then(|_| f.sync_all()) {
                err = Some(e);
                break;
            }
        }
        let err = err.expect("4 MiB written under a 1 MiB hard limit");
        assert_eq!(err.raw_os_error(), Some(libc::EDQUOT), "{err}");
        assert_eq!(crate::server::createattrs::io_status(&err), nextnfs_proto::nfs4_proto::NfsStat4::Nfs4errDquot);

        // Removing frees it.
        drop(f);
        let before = usage(&root).unwrap().unwrap().used_bytes;
        root.remove_file("/new").unwrap();
        let after = usage(&root).unwrap().unwrap().used_bytes;
        assert!(after < before, "remove freed nothing: {before} -> {after}");

        // A second start finds the project (no new walk), keeps the limits
        // when none are given, and a reopened root probes it.
        let again = ExportRoot::open(&dir.0).unwrap();
        assert_eq!(setup(&again, &QuotaLimits::default()).unwrap(), id);
        assert_eq!(usage(&again).unwrap().unwrap().hard_bytes, 1 << 20);
        let probed = ExportRoot::open(&dir.0).unwrap();
        probe(&probed);
        assert_eq!(probed.quota_project(), Some(id));
        // Another id for the same directory is refused.
        assert!(setup(&again, &QuotaLimits { project_id: Some(id + 1), ..Default::default() }).is_err());
        // Clear the limits so the test filesystem is left as it was.
        setup(&again, &QuotaLimits { hard_bytes: Some(0), soft_bytes: Some(0), project_id: None }).unwrap();
    }
}
