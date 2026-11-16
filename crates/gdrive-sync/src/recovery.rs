use std::ffi::CString;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub(crate) const DIRECTORY: &str = ".gdrive-recovery";
const MARKER: &str = "g-client local recovery v1\n";

pub(crate) fn validate(directory: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir()
        || std::fs::symlink_metadata(directory.join("owner"))?
            .file_type()
            .is_symlink()
        || std::fs::read_to_string(directory.join("owner"))? != MARKER
    {
        return Err(std::io::Error::other(
            "reserved .gdrive-recovery path is not a g-client recovery directory",
        ));
    }
    Ok(())
}

pub(crate) fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|source| std::io::Error::new(std::io::ErrorKind::InvalidInput, source))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|source| std::io::Error::new(std::io::ErrorKind::InvalidInput, source))?;
    let result = unsafe {
        nix::libc::renameat2(
            nix::libc::AT_FDCWD,
            from.as_ptr(),
            nix::libc::AT_FDCWD,
            to.as_ptr(),
            nix::libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub(crate) fn archive(path: &Path) -> std::io::Result<Option<PathBuf>> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error),
    }
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("recovery path has no parent"))?;
    let directory = parent.join(DIRECTORY);
    match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {
            let mut marker = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(directory.join("owner"))?;
            marker.write_all(MARKER.as_bytes())?;
            marker.sync_all()?;
            std::fs::File::open(&directory)?.sync_all()?;
            std::fs::File::open(parent)?.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => validate(&directory)?,
        Err(error) => return Err(error),
    }
    let entry = directory.join(uuid::Uuid::new_v4().to_string());
    std::fs::DirBuilder::new().mode(0o700).create(&entry)?;
    std::fs::File::open(&directory)?.sync_all()?;
    let destination = entry.join(
        path.file_name()
            .ok_or_else(|| std::io::Error::other("recovery path has no filename"))?,
    );
    match rename_no_replace(path, &destination) {
        Ok(()) => {
            std::fs::File::open(&entry)?.sync_all()?;
            std::fs::File::open(parent)?.sync_all()?;
            Ok(Some(destination))
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            std::fs::remove_dir(&entry)?;
            Ok(None)
        }
        Err(error) => {
            if let Err(cleanup) = std::fs::remove_dir(&entry) {
                tracing::warn!(%cleanup, "could not remove empty recovery entry");
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archived_files_and_directories_retain_writes_through_open_handles() {
        let root = std::env::temp_dir().join(format!("gdrive-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        for directory in [false, true] {
            let path = root.join(if directory { "folder" } else { "file" });
            let file = if directory {
                std::fs::create_dir(&path).unwrap();
                path.join("child")
            } else {
                path.clone()
            };
            let mut writer = std::fs::File::create(&file).unwrap();
            writer.write_all(b"before").unwrap();
            let archived = archive(&path).unwrap().unwrap();
            writer.write_all(b"-late-edit").unwrap();
            let archived_file = if directory {
                archived.join("child")
            } else {
                archived
            };
            assert_eq!(std::fs::read(archived_file).unwrap(), b"before-late-edit");
            assert!(!path.exists());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unmanaged_or_symlinked_recovery_paths_fail_without_moving_user_files() {
        let root = std::env::temp_dir().join(format!("gdrive-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("file");
        std::fs::write(&path, b"keep").unwrap();
        let reserved = root.join(DIRECTORY);
        std::fs::create_dir(&reserved).unwrap();
        assert!(archive(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
        std::fs::remove_dir(&reserved).unwrap();
        std::os::unix::fs::symlink(&root, &reserved).unwrap();
        assert!(archive(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
        std::fs::remove_dir_all(root).unwrap();
    }
}
