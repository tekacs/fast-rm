use std::ffi::{CStr, CString};
use std::fmt;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow};
use rayon::Scope;

#[derive(Clone, Copy, Debug)]
pub struct PurgeOptions {
    pub jobs: usize,
    pub cross_device: bool,
}

#[derive(Debug)]
pub struct PurgeReport {
    pub files_removed: u64,
    pub dirs_removed: u64,
    pub skipped: u64,
    pub errors: Vec<PurgeError>,
}

#[derive(Debug)]
pub struct PurgeError {
    path: PathBuf,
    operation: &'static str,
    message: String,
}

impl PurgeError {
    fn new(path: PathBuf, operation: &'static str, error: impl fmt::Display) -> Self {
        Self {
            path,
            operation,
            message: error.to_string(),
        }
    }
}

impl fmt::Display for PurgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.operation,
            self.path.display(),
            self.message
        )
    }
}

struct PurgeContext {
    options: PurgeOptions,
    files_removed: AtomicU64,
    dirs_removed: AtomicU64,
    skipped: AtomicU64,
    errors: Mutex<Vec<PurgeError>>,
}

enum EntryKind {
    Directory,
    NonDirectory,
    Unknown,
}

struct DirStream(*mut libc::DIR);

impl Drop for DirStream {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

pub fn purge_paths(paths: Vec<PathBuf>, options: PurgeOptions) -> Result<PurgeReport> {
    if options.jobs == 0 {
        return Err(anyhow!("purge jobs must be greater than zero"));
    }

    let context = PurgeContext {
        options,
        files_removed: AtomicU64::new(0),
        dirs_removed: AtomicU64::new(0),
        skipped: AtomicU64::new(0),
        errors: Mutex::new(Vec::new()),
    };

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(options.jobs)
        .build()
        .map_err(|error| anyhow!("failed to build purge thread pool: {error}"))?;

    pool.install(|| {
        rayon::scope(|scope| {
            for path in paths {
                let context = &context;
                scope.spawn(move |_| purge_path(path, context));
            }
        });
    });

    Ok(PurgeReport {
        files_removed: context.files_removed.load(Ordering::Relaxed),
        dirs_removed: context.dirs_removed.load(Ordering::Relaxed),
        skipped: context.skipped.load(Ordering::Relaxed),
        errors: context
            .errors
            .into_inner()
            .map_err(|_| anyhow!("purge error ledger was poisoned"))?,
    })
}

fn purge_path(path: PathBuf, context: &PurgeContext) {
    if path.as_os_str().is_empty() {
        context.record(path, "refuse", "empty path");
        return;
    }

    if path.parent().is_none() {
        context.record(path, "refuse", "refusing to purge filesystem root");
        return;
    }

    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            context.record(path, "stat", error);
            return;
        }
    };

    let file_type = metadata.file_type();
    if file_type.is_symlink() || !file_type.is_dir() {
        match std::fs::remove_file(&path) {
            Ok(()) => {
                context.files_removed.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => context.record(path, "unlink", error),
        }
        return;
    }

    let root_dev = metadata.dev();
    let fd = match open_dir_path(&path) {
        Ok(fd) => fd,
        Err(error) => {
            context.record(path, "open-dir", error);
            return;
        }
    };

    purge_open_dir(fd, path.clone(), root_dev, context);

    match std::fs::remove_dir(&path) {
        Ok(()) => {
            context.dirs_removed.fetch_add(1, Ordering::Relaxed);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => context.record(path, "rmdir", error),
    }
}

fn purge_open_dir(fd: OwnedFd, display_path: PathBuf, root_dev: u64, context: &PurgeContext) {
    let raw_fd = fd.as_raw_fd();

    rayon::scope(|scope| {
        let scan_fd = match dup_fd(raw_fd) {
            Ok(scan_fd) => scan_fd,
            Err(error) => {
                context.record(display_path.clone(), "dup-dir", error);
                return;
            }
        };

        let dir = match fdopendir(scan_fd) {
            Ok(dir) => dir,
            Err(error) => {
                unsafe {
                    libc::close(scan_fd);
                }
                context.record(display_path.clone(), "fdopendir", error);
                return;
            }
        };

        scan_dir(scope, &dir, raw_fd, &display_path, root_dev, context);
    });
}

fn scan_dir<'scope>(
    scope: &Scope<'scope>,
    dir: &DirStream,
    parent_fd: RawFd,
    display_path: &Path,
    root_dev: u64,
    context: &'scope PurgeContext,
) {
    let mut file_batch = Vec::with_capacity(256);

    loop {
        let entry = unsafe { libc::readdir(dir.0) };
        if entry.is_null() {
            flush_files(
                scope,
                parent_fd,
                display_path.to_path_buf(),
                &mut file_batch,
                context,
            );
            return;
        }

        let name_bytes = unsafe {
            let name = CStr::from_ptr((*entry).d_name.as_ptr());
            name.to_bytes()
        };

        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }

        let kind = dirent_kind(unsafe { (*entry).d_type });
        match kind {
            EntryKind::Directory => {
                flush_files(
                    scope,
                    parent_fd,
                    display_path.to_path_buf(),
                    &mut file_batch,
                    context,
                );
                spawn_dir(
                    scope,
                    parent_fd,
                    name_bytes.to_vec(),
                    child_path(display_path, name_bytes),
                    root_dev,
                    context,
                );
            }
            EntryKind::NonDirectory => {
                file_batch.push(name_bytes.to_vec());
                if file_batch.len() >= 256 {
                    flush_files(
                        scope,
                        parent_fd,
                        display_path.to_path_buf(),
                        &mut file_batch,
                        context,
                    );
                }
            }
            EntryKind::Unknown => match stat_kind(parent_fd, name_bytes) {
                Ok(EntryKind::Directory) => {
                    flush_files(
                        scope,
                        parent_fd,
                        display_path.to_path_buf(),
                        &mut file_batch,
                        context,
                    );
                    spawn_dir(
                        scope,
                        parent_fd,
                        name_bytes.to_vec(),
                        child_path(display_path, name_bytes),
                        root_dev,
                        context,
                    );
                }
                Ok(EntryKind::NonDirectory | EntryKind::Unknown) => {
                    file_batch.push(name_bytes.to_vec());
                    if file_batch.len() >= 256 {
                        flush_files(
                            scope,
                            parent_fd,
                            display_path.to_path_buf(),
                            &mut file_batch,
                            context,
                        );
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    context.record(child_path(display_path, name_bytes), "fstatat", error)
                }
            },
        }
    }
}

fn spawn_dir<'scope>(
    scope: &Scope<'scope>,
    parent_fd: RawFd,
    name: Vec<u8>,
    display_path: PathBuf,
    root_dev: u64,
    context: &'scope PurgeContext,
) {
    scope.spawn(move |_| purge_child_dir(parent_fd, name, display_path, root_dev, context));
}

fn flush_files<'scope>(
    scope: &Scope<'scope>,
    parent_fd: RawFd,
    base_path: PathBuf,
    file_batch: &mut Vec<Vec<u8>>,
    context: &'scope PurgeContext,
) {
    if file_batch.is_empty() {
        return;
    }

    let batch = std::mem::take(file_batch);
    scope.spawn(move |_| {
        for name in batch {
            unlink_name(parent_fd, &name, &base_path, context);
        }
    });
}

fn purge_child_dir(
    parent_fd: RawFd,
    name: Vec<u8>,
    display_path: PathBuf,
    root_dev: u64,
    context: &PurgeContext,
) {
    let c_name = match CString::new(name.as_slice()) {
        Ok(name) => name,
        Err(error) => {
            context.record(display_path, "name", error);
            return;
        }
    };

    let fd = match open_dir_at(parent_fd, &c_name) {
        Ok(fd) => fd,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return,
        Err(error)
            if error.raw_os_error() == Some(libc::ENOTDIR)
                || error.raw_os_error() == Some(libc::ELOOP) =>
        {
            unlink_name(parent_fd, &name, &display_path, context);
            return;
        }
        Err(error) => {
            context.record(display_path, "open-dir", error);
            return;
        }
    };

    if !context.options.cross_device {
        match fd_dev(fd.as_raw_fd()) {
            Ok(dev) if dev != root_dev => {
                context.skipped.fetch_add(1, Ordering::Relaxed);
                context.record(display_path, "skip", "cross-device directory");
                return;
            }
            Ok(_) => {}
            Err(error) => {
                context.record(display_path, "fstat", error);
                return;
            }
        }
    }

    purge_open_dir(fd, display_path.clone(), root_dev, context);

    let result = unsafe { libc::unlinkat(parent_fd, c_name.as_ptr(), libc::AT_REMOVEDIR) };
    if result == 0 {
        context.dirs_removed.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let error = io::Error::last_os_error();
    if error.kind() != io::ErrorKind::NotFound {
        context.record(display_path, "rmdir", error);
    }
}

fn unlink_name(parent_fd: RawFd, name: &[u8], base_path: &Path, context: &PurgeContext) {
    let c_name = match CString::new(name) {
        Ok(name) => name,
        Err(error) => {
            context.record(child_path(base_path, name), "name", error);
            return;
        }
    };

    let result = unsafe { libc::unlinkat(parent_fd, c_name.as_ptr(), 0) };
    if result == 0 {
        context.files_removed.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let error = io::Error::last_os_error();
    if error.kind() != io::ErrorKind::NotFound {
        context.record(child_path(base_path, name), "unlink", error);
    }
}

fn dirent_kind(d_type: u8) -> EntryKind {
    match d_type {
        libc::DT_DIR => EntryKind::Directory,
        libc::DT_UNKNOWN => EntryKind::Unknown,
        _ => EntryKind::NonDirectory,
    }
}

fn stat_kind(parent_fd: RawFd, name: &[u8]) -> io::Result<EntryKind> {
    let c_name = CString::new(name)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent_fd,
            c_name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };

    if result != 0 {
        return Err(io::Error::last_os_error());
    }

    let stat = unsafe { stat.assume_init() };
    if (stat.st_mode & libc::S_IFMT) == libc::S_IFDIR {
        Ok(EntryKind::Directory)
    } else {
        Ok(EntryKind::NonDirectory)
    }
}

fn open_dir_path(path: &Path) -> io::Result<OwnedFd> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn open_dir_at(parent_fd: RawFd, name: &CStr) -> io::Result<OwnedFd> {
    let fd = unsafe {
        libc::openat(
            parent_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn dup_fd(fd: RawFd) -> io::Result<RawFd> {
    let new_fd = unsafe { libc::dup(fd) };
    if new_fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let result = unsafe { libc::fcntl(new_fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    if result < 0 {
        let error = io::Error::last_os_error();
        unsafe {
            libc::close(new_fd);
        }
        return Err(error);
    }

    Ok(new_fd)
}

fn fdopendir(fd: RawFd) -> io::Result<DirStream> {
    let dir = unsafe { libc::fdopendir(fd) };
    if dir.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(DirStream(dir))
    }
}

fn fd_dev(fd: RawFd) -> io::Result<u64> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    let result = unsafe { libc::fstat(fd, stat.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(dev_to_u64(unsafe { stat.assume_init() }.st_dev))
}

fn child_path(base_path: &Path, name: &[u8]) -> PathBuf {
    base_path.join(std::ffi::OsStr::from_bytes(name))
}

#[cfg(target_os = "macos")]
fn dev_to_u64(dev: libc::dev_t) -> u64 {
    dev as u64
}

#[cfg(not(target_os = "macos"))]
fn dev_to_u64(dev: libc::dev_t) -> u64 {
    dev
}

impl PurgeContext {
    fn record(&self, path: PathBuf, operation: &'static str, error: impl fmt::Display) {
        let mut errors = self.errors.lock().expect("purge error ledger poisoned");
        errors.push(PurgeError::new(path, operation, error));
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn purges_nested_tree() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::write(root.join("a/file-a"), "a").unwrap();
        fs::write(root.join("a/b/file-b"), "b").unwrap();
        fs::write(root.join("a/b/c/file-c"), "c").unwrap();

        let report = purge_paths(
            vec![root.clone()],
            PurgeOptions {
                jobs: 4,
                cross_device: false,
            },
        )
        .unwrap();

        assert!(!root.exists());
        assert_eq!(report.errors.len(), 0);
        assert_eq!(report.files_removed, 3);
        assert_eq!(report.dirs_removed, 4);
    }

    #[test]
    fn purges_symlink_without_touching_target() {
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("target");
        let root = tmp.path().join("root");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("kept"), "kept").unwrap();
        fs::create_dir_all(&root).unwrap();
        symlink(&target, root.join("link")).unwrap();

        let report = purge_paths(
            vec![root.clone()],
            PurgeOptions {
                jobs: 4,
                cross_device: false,
            },
        )
        .unwrap();

        assert!(!root.exists());
        assert!(target.join("kept").exists());
        assert_eq!(report.errors.len(), 0);
    }
}
