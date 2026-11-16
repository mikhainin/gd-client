//! Descriptor-rooted Linux filesystem operations without descendant symlink traversal.

use std::ffi::{CString, OsStr, OsString};
use std::fs::{File, FileType, Metadata, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

use gdrive_common::local_roots::{verify_case_sensitive_directory, verify_local_filesystem};
use nix::libc;

pub(crate) fn blocking_fs<T>(operation: impl FnOnce() -> T) -> T {
    // Filesystem work must finish before cancellation can release the root lease.
    if tokio::runtime::Handle::try_current()
        .is_ok_and(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
    {
        tokio::task::block_in_place(operation)
    } else {
        operation()
    }
}

pub(crate) struct LocalFs<'a>(pub &'a Path);

pub(crate) struct Directory(File);

pub(crate) struct DirectoryEntries {
    _directory: Directory,
    entries: std::fs::ReadDir,
}

impl DirectoryEntries {
    pub fn into_directory(self) -> Directory {
        self._directory
    }
}

impl Iterator for DirectoryEntries {
    type Item = io::Result<(OsString, FileType)>;

    fn next(&mut self) -> Option<Self::Item> {
        blocking_fs(|| {
            self.entries.next().map(|entry| {
                let entry = entry?;
                Ok((entry.file_name(), entry.file_type()?))
            })
        })
    }
}

impl Directory {
    pub fn metadata(&self) -> io::Result<Metadata> {
        self.0.metadata()
    }

    fn child(&self, name: &OsStr, create: bool) -> io::Result<Self> {
        let open = || open_at(self.0.as_raw_fd(), name, libc::O_RDONLY | libc::O_DIRECTORY);
        let file = match open() {
            Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                match std::fs::create_dir(self.path(name)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                open()?
            }
            result => result?,
        };
        verify_case_sensitive_directory(&file)?;
        Ok(Self(file))
    }

    fn descend(mut self, path: &Path, create: bool) -> io::Result<Self> {
        for component in path.components() {
            match component {
                Component::Normal(name) => self = self.child(name, create)?,
                Component::CurDir | Component::RootDir => {}
                _ => return Err(invalid_path()),
            }
        }
        Ok(self)
    }

    pub fn path(&self, name: &OsStr) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd())).join(name)
    }
}

fn invalid_path() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "local path must remain beneath its sync root",
    )
}

fn open_at(parent: RawFd, name: &OsStr, flags: i32) -> io::Result<File> {
    let name = CString::new(name.as_encoded_bytes())?;
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    let nonblock = if flags & libc::O_PATH == 0 {
        libc::O_NONBLOCK
    } else {
        0
    };
    how.flags = (flags | libc::O_CLOEXEC | nonblock) as u64;
    how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS;
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent,
            name.as_ptr(),
            &how,
            std::mem::size_of_val(&how),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd as RawFd) })
}

impl LocalFs<'_> {
    fn directory(&self, path: &Path, create: bool) -> io::Result<Directory> {
        if !self.0.is_absolute() {
            return Err(invalid_path());
        }
        let relative = path.strip_prefix(self.0).map_err(|_| invalid_path())?;
        let filesystem_root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/")?;
        verify_case_sensitive_directory(&filesystem_root)?;
        Directory(filesystem_root)
            .descend(self.0, create)?
            .descend(relative, create)
    }

    fn parent(&self, path: &Path) -> io::Result<(Directory, OsString)> {
        let relative = path.strip_prefix(self.0).map_err(|_| invalid_path())?;
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(invalid_path());
        }
        let name = relative.file_name().ok_or_else(invalid_path)?.to_owned();
        let parent = path.parent().ok_or_else(invalid_path)?;
        Ok((self.directory(parent, false)?, name))
    }

    fn file(&self, path: &Path, flags: i32) -> io::Result<File> {
        let (parent, name) = self.parent(path)?;
        let file = open_at(parent.0.as_raw_fd(), &name, flags)?;
        verify_local_filesystem(&file)?;
        let metadata = file.metadata()?;
        if metadata.is_dir() {
            let directory = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY)
                .open(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
            verify_case_sensitive_directory(&directory)?;
        } else if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "local symbolic links and special files are not supported",
            ));
        }
        Ok(file)
    }

    pub fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        blocking_fs(|| self.directory(path, true).map(|_| ()))
    }

    pub fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        blocking_fs(|| {
            if path == self.0 {
                self.directory(path, false)?.0.metadata()
            } else {
                self.file(path, libc::O_PATH | libc::O_NOFOLLOW)?.metadata()
            }
        })
    }

    pub fn try_exists(&self, path: &Path) -> io::Result<bool> {
        match self.metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.read_limited(path, u64::MAX)
    }

    pub fn read_limited(&self, path: &Path, limit: u64) -> io::Result<Vec<u8>> {
        blocking_fs(|| {
            let pinned = self.file(path, libc::O_PATH | libc::O_NOFOLLOW)?;
            if !pinned.metadata()?.is_file() {
                return Err(io::Error::other("local data must be a regular file"));
            }
            let file = File::open(format!("/proc/self/fd/{}", pinned.as_raw_fd()))?;
            let mut content = Vec::new();
            file.take(limit.saturating_add(1))
                .read_to_end(&mut content)?;
            if content.len() as u64 > limit {
                return Err(io::Error::other("local metadata is too large"));
            }
            Ok(content)
        })
    }

    pub fn read_dir(&self, path: &Path) -> io::Result<DirectoryEntries> {
        blocking_fs(|| {
            let directory = self.directory(path, false)?;
            let entries = std::fs::read_dir(directory.path(OsStr::new(".")))?;
            Ok(DirectoryEntries {
                _directory: directory,
                entries,
            })
        })
    }

    pub fn create_new(&self, path: &Path) -> io::Result<File> {
        blocking_fs(|| {
            let (parent, name) = self.parent(path)?;
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(parent.path(&name))
        })
    }

    pub fn rename_no_replace(&self, from: &Path, to: &Path) -> io::Result<()> {
        blocking_fs(|| {
            self.metadata(from)?;
            let (source, name) = self.parent(from)?;
            let (destination, new_name) = self.parent(to)?;
            let name = CString::new(name.as_encoded_bytes())?;
            let new_name = CString::new(new_name.as_encoded_bytes())?;
            if unsafe {
                libc::renameat2(
                    source.0.as_raw_fd(),
                    name.as_ptr(),
                    destination.0.as_raw_fd(),
                    new_name.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })
    }

    pub fn create_private_dir(&self, path: &Path) -> io::Result<()> {
        blocking_fs(|| {
            let (parent, name) = self.parent(path)?;
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(parent.path(&name))
        })
    }

    pub fn sync_dir(&self, path: &Path) -> io::Result<()> {
        blocking_fs(|| self.directory(path, false)?.0.sync_all())
    }

    pub fn remove_dir(&self, path: &Path) -> io::Result<()> {
        blocking_fs(|| {
            let (parent, name) = self.parent(path)?;
            std::fs::remove_dir(parent.path(&name))
        })
    }

    pub fn remove_file(&self, path: &Path) -> io::Result<()> {
        blocking_fs(|| {
            let (parent, name) = self.parent(path)?;
            std::fs::remove_file(parent.path(&name))
        })
    }

    pub fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        blocking_fs(|| {
            let (parent, name) = self.parent(path)?;
            std::fs::remove_dir_all(parent.path(&name))
        })
    }

    pub fn chown(
        &self,
        path: &Path,
        uid: nix::unistd::Uid,
        gid: nix::unistd::Gid,
    ) -> io::Result<()> {
        blocking_fs(|| {
            let file = self.file(path, libc::O_PATH | libc::O_NOFOLLOW)?;
            if unsafe {
                libc::fchownat(
                    file.as_raw_fd(),
                    c"".as_ptr(),
                    uid.as_raw(),
                    gid.as_raw(),
                    libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::symlink;

    #[test]
    fn descendant_links_cannot_redirect_reads_or_mutations() {
        let temporary =
            std::env::temp_dir().join(format!("gdrive-confined-{}", uuid::Uuid::new_v4()));
        let root = temporary.join("root");
        let other = temporary.join("other");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("sentinel"), b"keep").unwrap();
        std::fs::write(root.join("source"), b"local").unwrap();
        symlink(&other, root.join("sub")).unwrap();
        symlink(other.join("sentinel"), root.join("pointer")).unwrap();
        let fs = LocalFs(&root);
        assert!(fs.create_dir_all(&root.join("sub/new")).is_err());
        assert!(fs.read(&root.join("sub/sentinel")).is_err());
        assert!(fs.read(&root.join("pointer")).is_err());
        assert!(fs.metadata(&root.join("pointer")).is_err());
        assert!(fs.create_new(&root.join("sub/new")).is_err());
        assert!(fs
            .rename_no_replace(&root.join("source"), &root.join("sub/sentinel"))
            .is_err());
        assert!(fs.remove_file(&root.join("sub/sentinel")).is_err());
        assert!(fs.remove_dir_all(&root.join("sub/new")).is_err());
        assert!(fs
            .chown(
                &root.join("sub/sentinel"),
                nix::unistd::Uid::current(),
                nix::unistd::Gid::current()
            )
            .is_err());
        assert!(fs.create_dir_all(&root.join("../escaped")).is_err());
        assert_eq!(std::fs::read(other.join("sentinel")).unwrap(), b"keep");
        assert_eq!(std::fs::read(root.join("source")).unwrap(), b"local");
        assert_eq!(std::fs::read_dir(&other).unwrap().count(), 1);
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn parent_replacement_after_inspection_does_not_redirect_the_syscall() {
        let temporary =
            std::env::temp_dir().join(format!("gdrive-confined-{}", uuid::Uuid::new_v4()));
        let root = temporary.join("root");
        let other = temporary.join("other");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir(&other).unwrap();
        let fs = LocalFs(&root);
        let (parent, name) = fs.parent(&root.join("sub/file")).unwrap();
        std::fs::rename(root.join("sub"), root.join("moved")).unwrap();
        symlink(&other, root.join("sub")).unwrap();
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(parent.path(&name))
            .unwrap()
            .write_all(b"owned directory")
            .unwrap();
        assert_eq!(
            std::fs::read(root.join("moved/file")).unwrap(),
            b"owned directory"
        );
        assert!(!other.join("file").exists());
        assert!(fs.create_new(&root.join("sub/another")).is_err());
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn special_files_are_rejected_without_opening_them_for_data() {
        let root = std::env::temp_dir().join(format!("gdrive-confined-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let fifo = root.join("fifo");
        let name = CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert_eq!(
            LocalFs(&root).read(&fifo).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
