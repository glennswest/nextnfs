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
use std::sync::Arc;
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
        let root = ExportRoot { path, fd: Ok(fd) };
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

    /// truncate(2) the regular file at `path` to `size`.
    pub fn set_len(&self, path: &str, size: u64) -> io::Result<()> {
        self.open_file(path, libc::O_WRONLY | libc::O_NONBLOCK, 0)?.set_len(size)
    }
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
