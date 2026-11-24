//! Export confinement by descriptor (#123).
//!
//! Every path a client names is resolved from a directory fd held on the
//! export root with `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS |
//! RESOLVE_NO_MAGICLINKS)` (Linux >= 5.6), and the op then works on the
//! returned fd, or as an `*at` call on a confined fd of the parent directory
//! with a single-component name that is never followed. A path that leaves
//! the export (`..` above the root, an absolute path) fails with EXDEV and
//! any symlink on the way fails with ELOOP, at the moment of the op, so a
//! directory swapped for a symlink after `perm::check_confined` looked at it
//! (#122) cannot carry the op outside the export.
//!
//! `BeneathFS` puts the same resolver behind the `vfs` crate's `FileSystem`
//! trait, so the ops that go through `VfsPath` (READ, OPEN, READDIR,
//! REMOVE, ...) are confined too.

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, ErrorKind};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use vfs::error::VfsErrorKind;
use vfs::{FileSystem, SeekAndRead, SeekAndWrite, VfsError, VfsFileType, VfsMetadata, VfsResult};

/// `struct open_how` (linux/openat2.h). libc's is `#[non_exhaustive]`.
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;
const RESOLVE: u64 = RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS;

/// fchmodat2(2), Linux 6.6; the same number on every architecture.
const SYS_FCHMODAT2: libc::c_long = 452;

/// The empty path that AT_EMPTY_PATH calls take (a C string).
const EMPTY: &[u8] = b"\0";

fn cstr(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| io::Error::from(ErrorKind::InvalidInput))
}

fn check(ret: libc::c_int) -> io::Result<()> {
    if ret == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// `path` relative to the export root: a vfs path ("" or "/a/b") or a
/// filehandle path ("/" or "/a/b"). The root itself is ".".
fn rel(path: &str) -> &str {
    let r = path.trim_start_matches('/');
    if r.is_empty() { "." } else { r }
}

/// Split a relative path into its parent ("." at the top) and last name.
/// The root has no parent and no name: EINVAL.
fn split(path: &str) -> io::Result<(&str, &str)> {
    let r = rel(path).trim_end_matches('/');
    if r == "." || r.is_empty() {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let (parent, name) = match r.rfind('/') {
        Some(i) => (&r[..i], &r[i + 1..]),
        None => (".", r),
    };
    if name == "." || name == ".." || name.is_empty() {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    Ok((if parent.is_empty() { "." } else { parent }, name))
}

/// An O_PATH fd on the directory `path` (followed like `open(2)` does).
pub fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let c = cstr(path.as_os_str().as_bytes())?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Open the directory `rel` beneath the directory fd `root` in one
/// `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS)`
/// (#150): `..` above `root`, an absolute path or any symlink on the way
/// fails (EXDEV / ELOOP), so a component renamed or replaced between a
/// caller's check and this open cannot take it outside `root`. An empty
/// `rel` is `root` itself. Returns an O_PATH fd and the path the kernel
/// reports for it (`/proc/self/fd`), for messages.
pub fn open_dir_beneath(root: &OwnedFd, rel: &Path) -> io::Result<(OwnedFd, PathBuf)> {
    let r = rel.as_os_str().as_bytes();
    let c = cstr(if r.is_empty() { b"." } else { r })?;
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: RESOLVE,
    };
    let fd = loop {
        let ret = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                root.as_raw_fd(),
                c.as_ptr(),
                &how as *const OpenHow,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if ret >= 0 {
            break unsafe { OwnedFd::from_raw_fd(ret as RawFd) };
        }
        let e = io::Error::last_os_error();
        if !matches!(e.raw_os_error(), Some(libc::EAGAIN) | Some(libc::EINTR)) {
            return Err(e);
        }
    };
    let path = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
    Ok((fd, path))
}

/// Whether `e` from [`open_dir_beneath`] means the path left the root or
/// went through a symlink (EXDEV / ELOOP), not that it is missing or bad.
pub fn is_escape(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EXDEV) | Some(libc::ELOOP))
}

/// The longest kernel file handle that fits in an NFS handle: 26 bytes
/// less version, export id, handle type and length (#133).
pub const KERNEL_HANDLE_MAX: usize = 22;

/// `struct file_handle` (fcntl.h) with room for `KERNEL_HANDLE_MAX` bytes.
#[repr(C)]
struct RawFileHandle {
    handle_bytes: u32,
    handle_type: i32,
    f_handle: [u8; KERNEL_HANDLE_MAX],
}

/// A filesystem's own handle for an object, from `name_to_handle_at(2)`
/// (#133, finding fdfec7e60065): it names the object for as long as it
/// exists, across restarts of the server, and carries the inode generation,
/// so a new object that reuses the inode number has another handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelHandle {
    /// The filesystem's handle type (`FILEID_*`); at most 255.
    pub handle_type: u8,
    /// At most `KERNEL_HANDLE_MAX` bytes.
    pub bytes: Vec<u8>,
}

/// What an export root's filesystem gives for kernel handles (probed once).
#[derive(Debug, Clone, Copy)]
struct HandleSupport {
    /// The root's mount id: objects on another mount (a filesystem mounted
    /// inside the export) get no kernel handle. `None`: no kernel handles.
    mount_id: Option<i32>,
    /// Whether `open_by_handle_at(2)` works here (it needs
    /// CAP_DAC_READ_SEARCH): without it, a handle not in memory cannot be
    /// resolved, so handles do not survive a restart.
    resolvable: bool,
}

/// What `ExportRoot::statvfs` reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsSpace {
    pub bytes_total: u64,
    pub bytes_free: u64,
    pub bytes_avail: u64,
    pub files_total: u64,
    pub files_free: u64,
    pub files_avail: u64,
}

/// A directory fd on an export root that every path of the export is
/// resolved beneath.
#[derive(Debug)]
pub struct ExportRoot {
    path: PathBuf,
    /// The root fd, or the errno that opening it failed with: then every op
    /// fails with that error (closed, never unconfined).
    fd: Result<OwnedFd, i32>,
    /// Kernel handle support, probed on first use (#133).
    handles: OnceLock<HandleSupport>,
}

impl ExportRoot {
    /// Open `path` as an export root, and check that the kernel confines
    /// paths beneath it (openat2, Linux >= 5.6). An export is refused
    /// without it: there is no other race-free way to keep ops inside it.
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::from_fd(open_dir(path)?, path.to_path_buf())
    }

    /// An export root on a directory fd already opened (and checked) by
    /// the caller, e.g. by [`open_dir_beneath`] for an API-added export
    /// (#150): the export is that directory, whatever `path` names by the
    /// time it is used. `path` is for messages and the API only.
    pub fn from_fd(fd: OwnedFd, path: PathBuf) -> io::Result<Self> {
        let root = ExportRoot { path, fd: Ok(fd), handles: OnceLock::new() };
        match root.open_fd(".", libc::O_PATH | libc::O_DIRECTORY, 0) {
            Ok(_) => Ok(root),
            Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => Err(io::Error::new(
                ErrorKind::Unsupported,
                "the kernel has no openat2(2) (Linux >= 5.6 is needed to keep ops inside an export)",
            )),
            Err(e) => Err(e),
        }
    }

    /// `open`, or a root whose every op fails with the error it got.
    pub fn open_or_closed(path: &Path) -> Self {
        match Self::open(path) {
            Ok(root) => root,
            Err(e) => {
                tracing::error!(path = %path.display(), "export root: {e}; every op on it fails");
                ExportRoot {
                    path: path.to_path_buf(),
                    fd: Err(e.raw_os_error().unwrap_or(libc::EIO)),
                    handles: OnceLock::new(),
                }
            }
        }
    }

    /// The space and inode counts of the filesystem the export is on
    /// (fstatvfs of the root fd), as GETATTR reports them: bytes and inodes
    /// in total, free, and available to an unprivileged user.
    pub fn statvfs(&self) -> io::Result<FsSpace> {
        let fd = self.root_fd()?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        check(unsafe { libc::fstatvfs(fd, &mut st) })?;
        let frsize = st.f_frsize as u64;
        Ok(FsSpace {
            bytes_total: (st.f_blocks as u64).saturating_mul(frsize),
            bytes_free: (st.f_bfree as u64).saturating_mul(frsize),
            bytes_avail: (st.f_bavail as u64).saturating_mul(frsize),
            files_total: st.f_files as u64,
            files_free: st.f_ffree as u64,
            files_avail: st.f_favail as u64,
        })
    }

    /// The export root's path (for messages and the API; never opened by it).
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn root_fd(&self) -> io::Result<RawFd> {
        match &self.fd {
            Ok(fd) => Ok(fd.as_raw_fd()),
            Err(errno) => Err(io::Error::from_raw_os_error(*errno)),
        }
    }

    /// openat2 `path` beneath the root. `mode` is used only with O_CREAT.
    fn open_fd(&self, path: &str, flags: i32, mode: u32) -> io::Result<OwnedFd> {
        let root = self.root_fd()?;
        let c = cstr(rel(path).as_bytes())?;
        let how = OpenHow {
            flags: (flags | libc::O_CLOEXEC) as u64,
            mode: if flags & libc::O_CREAT != 0 { u64::from(mode) } else { 0 },
            resolve: RESOLVE,
        };
        loop {
            let ret = unsafe {
                libc::syscall(
                    libc::SYS_openat2,
                    root,
                    c.as_ptr(),
                    &how as *const OpenHow,
                    std::mem::size_of::<OpenHow>(),
                )
            };
            if ret >= 0 {
                return Ok(unsafe { OwnedFd::from_raw_fd(ret as RawFd) });
            }
            let e = io::Error::last_os_error();
            // EAGAIN: a rename raced the lookup (RESOLVE_BENEATH retries).
            if !matches!(e.raw_os_error(), Some(libc::EAGAIN) | Some(libc::EINTR)) {
                return Err(e);
            }
        }
    }

    /// An O_PATH fd on the object at `path` itself (a symlink too, not what
    /// it points to).
    fn open_path(&self, path: &str) -> io::Result<OwnedFd> {
        self.open_fd(path, libc::O_PATH | libc::O_NOFOLLOW, 0)
    }

    /// The parent directory of `path` (confined) and its last name.
    fn parent(&self, path: &str) -> io::Result<(OwnedFd, CString)> {
        let (dir, name) = split(path)?;
        let dfd = self.open_fd(dir, libc::O_PATH | libc::O_DIRECTORY, 0)?;
        Ok((dfd, cstr(name.as_bytes())?))
    }

    /// Open the file at `path` with `flags` (O_NOFOLLOW is implied: a
    /// symlink is never opened through). `mode` applies with O_CREAT.
    pub fn open_file(&self, path: &str, flags: i32, mode: u32) -> io::Result<File> {
        Ok(File::from(self.open_fd(path, flags | libc::O_NOFOLLOW, mode)?))
    }

    /// lstat(2) of `path`.
    pub fn symlink_metadata(&self, path: &str) -> io::Result<std::fs::Metadata> {
        File::from(self.open_path(path)?).metadata()
    }

    pub fn exists(&self, path: &str) -> bool {
        self.open_path(path).is_ok()
    }

    pub fn is_dir(&self, path: &str) -> bool {
        self.symlink_metadata(path).is_ok_and(|m| m.is_dir())
    }

    pub fn create_dir(&self, path: &str, mode: u32) -> io::Result<()> {
        let (dfd, name) = self.parent(path)?;
        check(unsafe { libc::mkdirat(dfd.as_raw_fd(), name.as_ptr(), mode as libc::mode_t) })
    }

    pub fn remove_file(&self, path: &str) -> io::Result<()> {
        let (dfd, name) = self.parent(path)?;
        check(unsafe { libc::unlinkat(dfd.as_raw_fd(), name.as_ptr(), 0) })
    }

    pub fn remove_dir(&self, path: &str) -> io::Result<()> {
        let (dfd, name) = self.parent(path)?;
        check(unsafe { libc::unlinkat(dfd.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) })
    }

    /// rename(2) `from` to `to`, both inside the export.
    pub fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let (sfd, sname) = self.parent(from)?;
        let (dfd, dname) = self.parent(to)?;
        check(unsafe {
            libc::renameat(sfd.as_raw_fd(), sname.as_ptr(), dfd.as_raw_fd(), dname.as_ptr())
        })
    }

    /// A hard link `to` of the object at `from` (a symlink is linked itself).
    pub fn hard_link(&self, from: &str, to: &str) -> io::Result<()> {
        let (sfd, sname) = self.parent(from)?;
        let (dfd, dname) = self.parent(to)?;
        check(unsafe {
            libc::linkat(sfd.as_raw_fd(), sname.as_ptr(), dfd.as_raw_fd(), dname.as_ptr(), 0)
        })
    }

    /// A symlink at `path` holding `target` (stored as given, never resolved
    /// by the server).
    pub fn symlink(&self, target: &OsStr, path: &str) -> io::Result<()> {
        let (dfd, name) = self.parent(path)?;
        let t = cstr(target.as_bytes())?;
        check(unsafe { libc::symlinkat(t.as_ptr(), dfd.as_raw_fd(), name.as_ptr()) })
    }

    pub fn read_link(&self, path: &str) -> io::Result<PathBuf> {
        let (dfd, name) = self.parent(path)?;
        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        let n = unsafe {
            libc::readlinkat(dfd.as_raw_fd(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        buf.truncate(n as usize);
        Ok(PathBuf::from(OsString::from_vec(buf)))
    }

    /// The names in the directory at `path`, without `.` and `..`.
    pub fn read_dir(&self, path: &str) -> io::Result<Vec<OsString>> {
        let fd = self.open_fd(path, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        // fdopendir takes the fd; closedir closes it.
        let raw = fd.into_raw_fd();
        let dir = unsafe { libc::fdopendir(raw) };
        if dir.is_null() {
            let e = io::Error::last_os_error();
            unsafe { libc::close(raw) };
            return Err(e);
        }
        let mut names = Vec::new();
        loop {
            let ent = unsafe { libc::readdir(dir) };
            if ent.is_null() {
                break;
            }
            let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                names.push(OsString::from_vec(name.to_vec()));
            }
        }
        unsafe { libc::closedir(dir) };
        Ok(names)
    }

    /// chown(2) the object at `path` itself (a symlink is not followed).
    /// `u32::MAX` leaves the uid or gid as it is.
    pub fn lchown(&self, path: &str, uid: u32, gid: u32) -> io::Result<()> {
        let fd = self.open_path(path)?;
        check(unsafe {
            libc::fchownat(fd.as_raw_fd(), EMPTY.as_ptr() as *const libc::c_char, uid, gid, libc::AT_EMPTY_PATH)
        })
    }

    /// chmod(2) the object at `path`; ELOOP when it is a symlink (a mode is
    /// never set through one).
    pub fn chmod(&self, path: &str, mode: u32) -> io::Result<()> {
        let fd = self.open_path(path)?;
        let file = File::from(fd);
        if file.metadata()?.file_type().is_symlink() {
            return Err(io::Error::from_raw_os_error(libc::ELOOP));
        }
        let raw = file.as_raw_fd();
        let ret = unsafe {
            libc::syscall(SYS_FCHMODAT2, raw, EMPTY.as_ptr() as *const libc::c_char, mode as libc::mode_t, libc::AT_EMPTY_PATH)
        };
        if ret == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if !matches!(e.raw_os_error(), Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::EOPNOTSUPP)) {
            return Err(e);
        }
        // Before Linux 6.6: chmod the inode the O_PATH fd holds through its
        // /proc entry (what glibc and musl do for AT_SYMLINK_NOFOLLOW).
        let proc = cstr(format!("/proc/self/fd/{raw}").as_bytes())?;
        check(unsafe { libc::chmod(proc.as_ptr(), mode as libc::mode_t) })
    }

    /// Set the access and modification times of `path` (a symlink itself).
    /// `None` is the server's current time.
    pub fn set_times(&self, path: &str, atime: Option<SystemTime>, mtime: Option<SystemTime>) -> io::Result<()> {
        let ts = |t: Option<SystemTime>| match t {
            None => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW },
            Some(t) => {
                let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
                libc::timespec {
                    tv_sec: d.as_secs() as libc::time_t,
                    tv_nsec: d.subsec_nanos() as _,
                }
            }
        };
        let times = [ts(atime), ts(mtime)];
        match split(path) {
            Ok(_) => {
                let (dfd, name) = self.parent(path)?;
                check(unsafe {
                    libc::utimensat(dfd.as_raw_fd(), name.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW)
                })
            }
            // The export root itself.
            Err(_) => {
                let fd = self.open_fd(".", libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
                check(unsafe { libc::futimens(fd.as_raw_fd(), times.as_ptr()) })
            }
        }
    }

    /// Set the time that is `Some` and leave the other as it is.
    fn set_one_time(&self, path: &str, atime: Option<SystemTime>, mtime: Option<SystemTime>) -> io::Result<()> {
        let m = self.symlink_metadata(path)?;
        let atime = atime.or_else(|| m.accessed().ok());
        let mtime = mtime.or_else(|| m.modified().ok());
        self.set_times(path, atime, mtime)
    }

    /// getxattr(2) `name` of the object at `path` (not through a symlink:
    /// a symlink is ELOOP) into a buffer of at most `max` bytes. The O_PATH
    /// fd is read through its /proc entry (fgetxattr refuses O_PATH fds).
    pub fn get_xattr(&self, path: &str, name: &str, max: usize) -> io::Result<Vec<u8>> {
        let file = File::from(self.open_path(path)?);
        if file.metadata()?.file_type().is_symlink() {
            return Err(io::Error::from_raw_os_error(libc::ELOOP));
        }
        let proc = cstr(format!("/proc/self/fd/{}", file.as_raw_fd()).as_bytes())?;
        let cname = cstr(name.as_bytes())?;
        let mut buf = vec![0u8; max];
        let n = unsafe {
            libc::getxattr(proc.as_ptr(), cname.as_ptr(), buf.as_mut_ptr() as *mut libc::c_void, max)
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        buf.truncate(n as usize);
        Ok(buf)
    }

    /// `name_to_handle_at(2)` on an fd: the handle and the mount id, or
    /// `None` when the filesystem has none or it is longer than
    /// `KERNEL_HANDLE_MAX` (or its type above 255).
    fn handle_of_fd(fd: RawFd) -> io::Result<Option<(KernelHandle, i32)>> {
        let mut fh = RawFileHandle {
            handle_bytes: KERNEL_HANDLE_MAX as u32,
            handle_type: 0,
            f_handle: [0; KERNEL_HANDLE_MAX],
        };
        let mut mount_id: libc::c_int = 0;
        let ret = unsafe {
            libc::syscall(
                libc::SYS_name_to_handle_at,
                fd,
                EMPTY.as_ptr() as *const libc::c_char,
                &mut fh as *mut RawFileHandle,
                &mut mount_id as *mut libc::c_int,
                libc::AT_EMPTY_PATH,
            )
        };
        if ret != 0 {
            let e = io::Error::last_os_error();
            return match e.raw_os_error() {
                // Too long, or the filesystem cannot make handles.
                Some(libc::EOVERFLOW) | Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS) => Ok(None),
                _ => Err(e),
            };
        }
        let len = fh.handle_bytes as usize;
        if len > KERNEL_HANDLE_MAX || !(0..=255).contains(&fh.handle_type) {
            return Ok(None);
        }
        Ok(Some((
            KernelHandle { handle_type: fh.handle_type as u8, bytes: fh.f_handle[..len].to_vec() },
            mount_id,
        )))
    }

    fn handle_support(&self) -> HandleSupport {
        *self.handles.get_or_init(|| {
            let none = HandleSupport { mount_id: None, resolvable: false };
            let Ok(fd) = self.open_path(".") else { return none };
            let Ok(Some((h, mount_id))) = Self::handle_of_fd(fd.as_raw_fd()) else {
                tracing::warn!(
                    path = %self.path.display(),
                    "export filesystem has no kernel file handles that fit an NFS handle: \
                     handles are inode numbers and do not survive a restart"
                );
                return none;
            };
            let resolvable = match self.open_by_kernel_handle(&h) {
                Ok(_) => true,
                Err(e) => {
                    tracing::warn!(
                        path = %self.path.display(),
                        "open_by_handle_at: {e} (it needs CAP_DAC_READ_SEARCH): \
                         handles do not survive a restart"
                    );
                    false
                }
            };
            HandleSupport { mount_id: Some(mount_id), resolvable }
        })
    }

    /// Whether handles of this export resolve after a restart: the
    /// filesystem has kernel handles and the server may open them (#133).
    pub fn persistent_handles(&self) -> bool {
        let s = self.handle_support();
        s.mount_id.is_some() && s.resolvable
    }

    /// The kernel handle of the object at `path` itself (a symlink too).
    /// `Ok(None)`: the object has none that fits, or it is on another
    /// mount than the export root (a filesystem mounted inside the export:
    /// a handle is decoded on the root's mount only).
    pub fn kernel_handle(&self, path: &str) -> io::Result<Option<KernelHandle>> {
        let Some(root_mount) = self.handle_support().mount_id else { return Ok(None) };
        let fd = self.open_path(path)?;
        Ok(match Self::handle_of_fd(fd.as_raw_fd())? {
            Some((h, mount_id)) if mount_id == root_mount => Some(h),
            _ => None,
        })
    }

    /// `open_by_handle_at(2)` on the root's filesystem: an O_PATH fd on the
    /// object, wherever it is on that filesystem (not checked here).
    fn open_by_kernel_handle(&self, h: &KernelHandle) -> io::Result<OwnedFd> {
        if h.bytes.len() > KERNEL_HANDLE_MAX {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        let mut fh = RawFileHandle {
            handle_bytes: h.bytes.len() as u32,
            handle_type: i32::from(h.handle_type),
            f_handle: [0; KERNEL_HANDLE_MAX],
        };
        fh.f_handle[..h.bytes.len()].copy_from_slice(&h.bytes);
        // The mount fd may not be an O_PATH fd (EBADF).
        let mount = self.open_fd(".", libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        let ret = unsafe {
            libc::syscall(
                libc::SYS_open_by_handle_at,
                mount.as_raw_fd(),
                &mut fh as *mut RawFileHandle,
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(ret as RawFd) })
    }

    /// The export path ("/" or "/a/b") of the object a kernel handle names,
    /// after a restart or whenever the handle is not in memory (#133).
    /// The object must be inside this export: its path, as the kernel
    /// reports it, must be beneath the root's, and opening that path
    /// beneath the root (no `..`, no symlinks) must reach the same object
    /// (dev:ino). Anything else is ESTALE: an object outside the export,
    /// one removed, or one whose path the kernel cannot report (a
    /// non-directory decoded without a cached name). EPERM without
    /// CAP_DAC_READ_SEARCH.
    pub fn resolve_kernel_handle(&self, h: &KernelHandle) -> io::Result<String> {
        let stale = || io::Error::from_raw_os_error(libc::ESTALE);
        let obj = File::from(self.open_by_kernel_handle(h)?);
        let root_path = std::fs::read_link(format!("/proc/self/fd/{}", self.root_fd()?))?;
        let obj_path = std::fs::read_link(format!("/proc/self/fd/{}", obj.as_raw_fd()))?;
        let rel = beneath_path(&root_path, &obj_path).ok_or_else(stale)?;
        let found = obj.metadata()?;
        let at = self.symlink_metadata(&rel).map_err(|_| stale())?;
        use std::os::unix::fs::MetadataExt;
        if (found.dev(), found.ino()) != (at.dev(), at.ino()) {
            return Err(stale());
        }
        Ok(rel)
    }

    /// truncate(2) the regular file at `path` to `size`.
    pub fn set_len(&self, path: &str, size: u64) -> io::Result<()> {
        self.open_file(path, libc::O_WRONLY | libc::O_NONBLOCK, 0)?.set_len(size)
    }
}

/// The export path ("/" or "/a/b") of `obj` when it is `root` or beneath
/// it, both as `/proc/self/fd` reports them. `None` outside it, for a
/// deleted object (" (deleted)" is not a name the export has) and for a
/// path that is not UTF-8 (export paths are strings).
fn beneath_path(root: &Path, obj: &Path) -> Option<String> {
    let rest = obj.strip_prefix(root).ok()?;
    let rest = rest.to_str()?;
    if rest.is_empty() {
        return Some("/".to_string());
    }
    if rest.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return None;
    }
    Some(format!("/{rest}"))
}

/// The `vfs` crate's view of an export, every path resolved beneath its
/// root (replaces `AltrootFS` over `PhysicalFS`, which joined paths).
#[derive(Debug)]
pub struct BeneathFS {
    root: Arc<ExportRoot>,
}

impl BeneathFS {
    pub fn new(root: Arc<ExportRoot>) -> Self {
        BeneathFS { root }
    }
}

fn vfs_err(e: io::Error) -> VfsError {
    VfsError::from(VfsErrorKind::IoError(e))
}

impl FileSystem for BeneathFS {
    fn read_dir(&self, path: &str) -> VfsResult<Box<dyn Iterator<Item = String> + Send>> {
        let names = self.root.read_dir(path).map_err(vfs_err)?;
        Ok(Box::new(
            names.into_iter().map(|n| n.to_string_lossy().into_owned()),
        ))
    }

    fn create_dir(&self, path: &str) -> VfsResult<()> {
        match self.root.create_dir(path, 0o777) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                if self.root.is_dir(path) {
                    Err(VfsErrorKind::DirectoryExists.into())
                } else {
                    Err(VfsErrorKind::FileExists.into())
                }
            }
            Err(e) => Err(vfs_err(e)),
        }
    }

    fn open_file(&self, path: &str) -> VfsResult<Box<dyn SeekAndRead + Send>> {
        let f = self.root.open_file(path, libc::O_RDONLY, 0).map_err(vfs_err)?;
        Ok(Box::new(f))
    }

    fn create_file(&self, path: &str) -> VfsResult<Box<dyn SeekAndWrite + Send>> {
        let f = self
            .root
            .open_file(path, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, 0o666)
            .map_err(vfs_err)?;
        Ok(Box::new(f))
    }

    fn append_file(&self, path: &str) -> VfsResult<Box<dyn SeekAndWrite + Send>> {
        let f = self.root.open_file(path, libc::O_WRONLY | libc::O_APPEND, 0).map_err(vfs_err)?;
        Ok(Box::new(f))
    }

    /// lstat: a symlink is reported as itself (a file), never followed.
    fn metadata(&self, path: &str) -> VfsResult<VfsMetadata> {
        let m = self.root.symlink_metadata(path).map_err(vfs_err)?;
        Ok(VfsMetadata {
            file_type: if m.is_dir() { VfsFileType::Directory } else { VfsFileType::File },
            len: if m.is_dir() { 0 } else { m.len() },
            created: m.created().ok(),
            modified: m.modified().ok(),
            accessed: m.accessed().ok(),
        })
    }

    fn set_modification_time(&self, path: &str, time: SystemTime) -> VfsResult<()> {
        self.root.set_one_time(path, None, Some(time)).map_err(vfs_err)
    }

    fn set_access_time(&self, path: &str, time: SystemTime) -> VfsResult<()> {
        self.root.set_one_time(path, Some(time), None).map_err(vfs_err)
    }

    fn exists(&self, path: &str) -> VfsResult<bool> {
        Ok(self.root.exists(path))
    }

    fn remove_file(&self, path: &str) -> VfsResult<()> {
        self.root.remove_file(path).map_err(vfs_err)
    }

    fn remove_dir(&self, path: &str) -> VfsResult<()> {
        self.root.remove_dir(path).map_err(vfs_err)
    }

    fn copy_file(&self, src: &str, dest: &str) -> VfsResult<()> {
        let mut from = self.root.open_file(src, libc::O_RDONLY, 0).map_err(vfs_err)?;
        let mut to = self
            .root
            .open_file(dest, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, 0o666)
            .map_err(vfs_err)?;
        io::copy(&mut from, &mut to).map_err(vfs_err)?;
        Ok(())
    }

    fn move_file(&self, src: &str, dest: &str) -> VfsResult<()> {
        self.root.rename(src, dest).map_err(vfs_err)
    }

    fn move_dir(&self, src: &str, dest: &str) -> VfsResult<()> {
        self.root.rename(src, dest).map_err(vfs_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
    use vfs::VfsPath;

    fn setup() -> (tempfile::TempDir, tempfile::TempDir, Arc<ExportRoot>) {
        let export = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"outside").unwrap();
        let root = Arc::new(ExportRoot::open(export.path()).unwrap());
        (export, outside, root)
    }

    #[test]
    fn test_beneath_path() {
        let p = |r: &str, o: &str| beneath_path(Path::new(r), Path::new(o));
        assert_eq!(p("/e", "/e").as_deref(), Some("/"));
        assert_eq!(p("/e", "/e/a/b").as_deref(), Some("/a/b"));
        assert_eq!(p("/", "/a").as_deref(), Some("/a"));
        assert_eq!(p("/e", "/ex/a"), None);
        assert_eq!(p("/e/a", "/e"), None);
        assert_eq!(p("/e", "/other/e"), None);
    }

    /// #133: the kernel handle of an object is the same for each lookup,
    /// differs between objects (a symlink is its own object), and differs
    /// for a file removed and created again at the same path.
    #[test]
    fn test_kernel_handle_names_the_object() {
        let (export, _outside, root) = setup();
        std::fs::write(export.path().join("a"), b"a").unwrap();
        std::fs::write(export.path().join("b"), b"b").unwrap();
        std::os::unix::fs::symlink("a", export.path().join("l")).unwrap();
        let Some(a) = root.kernel_handle("/a").unwrap() else {
            eprintln!("no kernel handles on {:?}", export.path());
            return;
        };
        assert!(a.bytes.len() <= KERNEL_HANDLE_MAX);
        assert_eq!(root.kernel_handle("/a").unwrap(), Some(a.clone()));
        assert_ne!(root.kernel_handle("/b").unwrap(), Some(a.clone()));
        assert_ne!(root.kernel_handle("/l").unwrap(), Some(a.clone()));
        assert!(root.kernel_handle("/").unwrap().is_some());
        std::fs::remove_file(export.path().join("a")).unwrap();
        assert_eq!(root.kernel_handle("/a").unwrap_err().kind(), ErrorKind::NotFound);
        std::fs::write(export.path().join("a"), b"again").unwrap();
        assert_ne!(root.kernel_handle("/a").unwrap(), Some(a));
    }

    /// #133: a kernel handle resolves only to an object inside the export.
    /// A handle of an object on the same filesystem outside the export,
    /// and one of a removed file, are ESTALE. Without CAP_DAC_READ_SEARCH
    /// nothing resolves (EPERM) and `persistent_handles` is false.
    #[test]
    fn test_resolve_kernel_handle_stays_inside() {
        let base = tempfile::tempdir().unwrap();
        let export = base.path().join("export");
        std::fs::create_dir_all(export.join("d")).unwrap();
        std::fs::create_dir(base.path().join("outside")).unwrap();
        std::fs::write(export.join("d/f"), b"in").unwrap();
        std::fs::write(export.join("gone"), b"x").unwrap();
        std::fs::write(base.path().join("outside/secret"), b"out").unwrap();
        let wide = ExportRoot::open(base.path()).unwrap();
        let root = ExportRoot::open(&export).unwrap();
        let (Some(inside), Some(secret), Some(gone)) = (
            root.kernel_handle("/d/f").unwrap(),
            wide.kernel_handle("/outside/secret").unwrap(),
            root.kernel_handle("/gone").unwrap(),
        ) else {
            eprintln!("no kernel handles on {:?}", base.path());
            return;
        };
        std::fs::remove_file(export.join("gone")).unwrap();
        if !root.persistent_handles() {
            let e = root.resolve_kernel_handle(&inside).unwrap_err();
            eprintln!("open_by_handle_at: {e}");
            assert!(root.resolve_kernel_handle(&secret).is_err());
            return;
        }
        assert_eq!(root.resolve_kernel_handle(&inside).unwrap(), "/d/f");
        assert_eq!(root.resolve_kernel_handle(&root.kernel_handle("/").unwrap().unwrap()).unwrap(), "/");
        let stale = |r: io::Result<String>| r.unwrap_err().raw_os_error() == Some(libc::ESTALE);
        assert!(stale(root.resolve_kernel_handle(&secret)));
        assert!(stale(root.resolve_kernel_handle(&gone)));
        // The wider export sees the same object at its own path.
        assert_eq!(wide.resolve_kernel_handle(&inside).unwrap(), "/export/d/f");
        // Moved within the export: found at the new path.
        std::fs::rename(export.join("d/f"), export.join("f2")).unwrap();
        assert_eq!(root.resolve_kernel_handle(&inside).unwrap(), "/f2");
        // Moved out of it: no longer this export's.
        std::fs::rename(export.join("f2"), base.path().join("outside/f2")).unwrap();
        assert!(stale(root.resolve_kernel_handle(&inside)));
    }

    #[test]
    fn test_split() {
        assert_eq!(split("/a").unwrap(), (".", "a"));
        assert_eq!(split("a/b/c").unwrap(), ("a/b", "c"));
        assert_eq!(split("/a/b/").unwrap(), ("a", "b"));
        assert!(split("/").is_err());
        assert!(split("").is_err());
        assert!(split("/a/..").is_err());
        assert_eq!(rel(""), ".");
        assert_eq!(rel("/x"), "x");
    }

    #[test]
    fn test_symlinked_dir_is_not_followed() {
        let (export, outside, root) = setup();
        std::os::unix::fs::symlink(outside.path(), export.path().join("d")).unwrap();
        for flags in [libc::O_RDONLY, libc::O_WRONLY, libc::O_RDWR | libc::O_APPEND] {
            let e = root.open_file("/d/secret", flags, 0).unwrap_err();
            assert_eq!(e.raw_os_error(), Some(libc::ELOOP), "{flags:#x}");
        }
        assert!(root.open_file("/d/new", libc::O_WRONLY | libc::O_CREAT, 0o644).is_err());
        assert!(root.remove_file("/d/secret").is_err());
        assert!(root.chmod("/d/secret", 0o777).is_err());
        assert!(root.lchown("/d/secret", u32::MAX, u32::MAX).is_err());
        assert!(root.set_len("/d/secret", 0).is_err());
        assert!(root.rename("/d/secret", "/stolen").is_err());
        assert!(root.hard_link("/d/secret", "/stolen").is_err());
        assert!(root.symlink(OsStr::new("x"), "/d/planted").is_err());
        assert!(root.read_dir("/d").is_err());
        assert!(root.create_dir("/d/sub", 0o755).is_err());
        assert_eq!(std::fs::read(outside.path().join("secret")).unwrap(), b"outside");
        assert!(!outside.path().join("new").exists());
        assert!(!outside.path().join("planted").exists());
        assert!(!export.path().join("stolen").exists());
    }

    #[test]
    fn test_symlink_object_is_itself() {
        let (export, outside, root) = setup();
        let secret = outside.path().join("secret");
        std::os::unix::fs::symlink(&secret, export.path().join("l")).unwrap();
        // Opened for data: refused. Metadata, readlink, chown: the link itself.
        assert_eq!(
            root.open_file("/l", libc::O_RDONLY, 0).unwrap_err().raw_os_error(),
            Some(libc::ELOOP)
        );
        assert!(root.symlink_metadata("/l").unwrap().file_type().is_symlink());
        assert_eq!(root.read_link("/l").unwrap(), secret);
        assert_eq!(root.chmod("/l", 0o777).unwrap_err().raw_os_error(), Some(libc::ELOOP));
        let past = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let secret_mtime = std::fs::metadata(&secret).unwrap().modified().unwrap();
        root.set_times("/l", Some(past), Some(past)).unwrap();
        assert_eq!(std::fs::metadata(&secret).unwrap().modified().unwrap(), secret_mtime);
        assert_eq!(std::fs::symlink_metadata(export.path().join("l")).unwrap().modified().unwrap(), past);
        // Unlink and rename work on the link, not its target.
        root.rename("/l", "/l2").unwrap();
        root.remove_file("/l2").unwrap();
        assert!(secret.exists());
    }

    #[test]
    fn test_escape_by_dotdot_and_absolute() {
        let (_export, _outside, root) = setup();
        let e = root.open_file("../x", libc::O_RDONLY, 0).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EXDEV));
        assert!(root.open_file("a/../../x", libc::O_RDONLY, 0).is_err());
    }

    #[test]
    fn test_ops_inside() {
        let (export, _outside, root) = setup();
        root.create_dir("/d", 0o755).unwrap();
        let f = root.open_file("/d/f", libc::O_WRONLY | libc::O_CREAT, 0o640).unwrap();
        f.write_all_at(b"hello world", 0).unwrap();
        root.set_len("/d/f", 5).unwrap();
        assert_eq!(std::fs::read(export.path().join("d/f")).unwrap(), b"hello");
        root.chmod("/d/f", 0o600).unwrap();
        assert_eq!(root.symlink_metadata("/d/f").unwrap().permissions().mode() & 0o7777, 0o600);
        root.hard_link("/d/f", "/g").unwrap();
        assert_eq!(root.symlink_metadata("/g").unwrap().nlink(), 2);
        root.rename("/g", "/d/h").unwrap();
        let mut names = root.read_dir("/d").unwrap();
        names.sort();
        assert_eq!(names, vec![OsString::from("f"), OsString::from("h")]);
        root.symlink(OsStr::new("/elsewhere"), "/s").unwrap();
        assert_eq!(root.read_link("/s").unwrap(), PathBuf::from("/elsewhere"));
        root.remove_file("/d/h").unwrap();
        root.remove_file("/d/f").unwrap();
        root.remove_dir("/d").unwrap();
        assert!(root.symlink_metadata("/").unwrap().is_dir());
        root.set_times("/", None, None).unwrap();
        root.lchown("/", u32::MAX, u32::MAX).unwrap();
    }

    #[test]
    fn test_beneath_fs() {
        let (export, outside, root) = setup();
        let fs: VfsPath = VfsPath::new(BeneathFS::new(root));
        fs.join("dir").unwrap().create_dir().unwrap();
        assert!(fs.join("dir").unwrap().create_dir().is_err());
        fs.join("dir/f").unwrap().create_file().unwrap().write_all(b"abc").unwrap();
        let mut s = String::new();
        fs.join("dir/f").unwrap().open_file().unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "abc");
        assert!(fs.join("dir").unwrap().is_dir().unwrap());
        assert_eq!(fs.join("dir/f").unwrap().metadata().unwrap().len, 3);
        let names: Vec<String> = fs.join("dir").unwrap().read_dir().unwrap().map(|p| p.filename()).collect();
        assert_eq!(names, vec!["f".to_string()]);
        fs.join("dir/f").unwrap().move_file(&fs.join("g").unwrap()).unwrap();
        assert!(export.path().join("g").exists());
        std::os::unix::fs::symlink(outside.path(), export.path().join("out")).unwrap();
        assert!(fs.join("out/secret").unwrap().open_file().is_err());
        assert!(fs.join("out/secret").unwrap().append_file().is_err());
        assert!(fs.join("out/new").unwrap().create_file().is_err());
        assert!(fs.join("out").unwrap().read_dir().is_err());
        assert!(!fs.join("out").unwrap().is_dir().unwrap());
        assert!(fs.join("out").unwrap().exists().unwrap());
        assert_eq!(std::fs::read(outside.path().join("secret")).unwrap(), b"outside");
        assert!(!outside.path().join("new").exists());
    }

    #[test]
    fn test_closed_root_fails_every_op() {
        let root = ExportRoot::open_or_closed(Path::new("/nonexistent/nextnfs-export"));
        assert!(root.open_file("/f", libc::O_RDONLY, 0).is_err());
        assert!(root.symlink_metadata("/").is_err());
        assert!(!root.exists("/"));
    }
}
