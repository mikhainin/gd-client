use std::ffi::CString;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub(crate) const DIRECTORY: &str = ".gdrive-recovery";
const MARKER: &str = "g-client local recovery v1\n";

pub(crate) fn validate(directory: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir()
        || read_regular(&directory.join("owner"), MARKER.len())? != MARKER.as_bytes()
    {
        return Err(std::io::Error::other(
            "reserved .gdrive-recovery path is not a g-client recovery directory",
        ));
    }
    Ok(())
}

fn read_regular(path: &Path, limit: usize) -> std::io::Result<Vec<u8>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other(
            "recovery metadata must be a regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(std::io::Error::other("recovery metadata is too large"));
    }
    Ok(bytes)
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

#[cfg(test)]
pub(crate) fn archive(path: &Path) -> std::io::Result<Option<PathBuf>> {
    archive_path(path, None)
}

pub(crate) fn archive_replacement(path: &Path) -> std::io::Result<Option<PathBuf>> {
    archive_path(path, Some(b'R'))
}

pub(crate) fn archive_removal(path: &Path) -> std::io::Result<Option<PathBuf>> {
    archive_path(path, Some(b'D'))
}

fn create_entry(path: &Path) -> std::io::Result<PathBuf> {
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
    Ok(entry)
}

fn write_intent(
    entry: &Path,
    path: &Path,
    kind: u8,
    target: Option<&Path>,
    version: Option<&str>,
) -> std::io::Result<()> {
    let directory = entry.parent().unwrap();
    let pending = directory.join("pending");
    match std::fs::DirBuilder::new().mode(0o700).create(&pending) {
        Ok(()) => {
            std::fs::File::open(directory)?.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if !std::fs::symlink_metadata(&pending)?.is_dir() {
                return Err(std::io::Error::other(
                    "recovery pending path must be a directory",
                ));
            }
        }
        Err(error) => return Err(error),
    }
    let temporary = entry.join(".intent");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let metadata = std::fs::symlink_metadata(path)?;
    file.write_all(&[kind])?;
    file.write_all(&metadata.dev().to_le_bytes())?;
    file.write_all(&metadata.ino().to_le_bytes())?;
    file.write_all(path.file_name().unwrap().as_bytes())?;
    if let Some(target) = target {
        file.write_all(&[0])?;
        file.write_all(target.as_os_str().as_bytes())?;
    }
    if kind == b'M' {
        file.write_all(&[0])?;
        match version {
            Some(version) => {
                file.write_all(b"1")?;
                file.write_all(version.as_bytes())?;
            }
            None => file.write_all(b"0")?,
        }
    }
    file.sync_all()?;
    drop(file);
    rename_no_replace(&temporary, &pending.join(entry.file_name().unwrap()))?;
    std::fs::File::open(&pending)?.sync_all()
}

// The returned handle identifies the journal; the data itself remains at `to`.
pub(crate) fn move_with_intent(
    root: &Path,
    from: &Path,
    to: &Path,
    version: Option<&str>,
) -> std::io::Result<PathBuf> {
    let relative = to
        .strip_prefix(root)
        .map_err(|source| std::io::Error::new(std::io::ErrorKind::InvalidInput, source))?;
    if !normal_path(relative) {
        return Err(std::io::Error::other("invalid recovery move target"));
    }
    let entry = create_entry(from)?;
    write_intent(&entry, from, b'M', Some(relative), version)?;
    if let Err(error) = rename_no_replace(from, to) {
        complete_replacement(&entry.join(from.file_name().unwrap()))?;
        return Err(error);
    }
    std::fs::File::open(from.parent().unwrap())?.sync_all()?;
    std::fs::File::open(to.parent().unwrap())?.sync_all()?;
    Ok(entry.join(from.file_name().unwrap()))
}

fn archive_path(path: &Path, intent: Option<u8>) -> std::io::Result<Option<PathBuf>> {
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
    let parent = path.parent().unwrap();
    let entry = create_entry(path)?;
    let destination = entry.join(
        path.file_name()
            .ok_or_else(|| std::io::Error::other("recovery path has no filename"))?,
    );
    if let Some(kind) = intent {
        write_intent(&entry, path, kind, None, None)?;
    }
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
            if intent.is_some() {
                complete_replacement(&destination)?;
            }
            std::fs::remove_dir(&entry)?;
            Ok(None)
        }
        Err(error) => {
            if intent.is_some() {
                complete_replacement(&destination)?;
            }
            if let Err(cleanup) = std::fs::remove_dir(&entry) {
                tracing::warn!(%cleanup, "could not remove empty recovery entry");
            }
            Err(error)
        }
    }
}

pub(crate) fn complete_replacement(archived: &Path) -> std::io::Result<()> {
    let entry = archived
        .parent()
        .ok_or_else(|| std::io::Error::other("invalid recovery entry"))?;
    let directory = entry
        .parent()
        .ok_or_else(|| std::io::Error::other("invalid recovery store"))?;
    let pending = directory.join("pending");
    match std::fs::remove_file(pending.join(entry.file_name().unwrap())) {
        Ok(()) => std::fs::File::open(&pending)?.sync_all(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn normal_path(path: &Path) -> bool {
    path.components().next().is_some() && path.components().all(|component|
        matches!(component, std::path::Component::Normal(name) if name != DIRECTORY))
}

pub(crate) fn restore_interrupted(
    root: &Path,
    tracked: impl Fn(&Path, Option<&str>) -> std::io::Result<bool>,
) -> std::io::Result<()> {
    restore_queued(root, vec![root.to_path_buf()], tracked)
}

fn restore_queued(
    root: &Path,
    mut directories: Vec<PathBuf>,
    tracked: impl Fn(&Path, Option<&str>) -> std::io::Result<bool>,
) -> std::io::Result<()> {
    while let Some(directory) = directories.pop() {
        let items = match std::fs::read_dir(&directory) {
            Ok(items) => items,
            Err(error)
                if directory != root
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        for item in items {
            let item = item?;
            if !item.file_type()?.is_dir() {
                continue;
            }
            if item.file_name() != DIRECTORY {
                directories.push(item.path());
                continue;
            }
            let store = item.path();
            validate(&store)?;
            let pending = store.join("pending");
            match std::fs::symlink_metadata(&pending) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => {
                    return Err(std::io::Error::other(
                        "recovery pending path must be a directory",
                    ))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
            for journal in std::fs::read_dir(&pending)? {
                let journal = journal?;
                let id = journal.file_name();
                let _id =
                    uuid::Uuid::parse_str(id.to_str().unwrap_or_default()).map_err(|source| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, source)
                    })?;
                let intent = read_regular(&journal.path(), 8192)?;
                if intent.len() < 18 {
                    return Err(std::io::Error::other("invalid recovery intent"));
                }
                let kind = intent[0];
                let device = u64::from_le_bytes(intent[1..9].try_into().unwrap());
                let inode = u64::from_le_bytes(intent[9..17].try_into().unwrap());
                let content = &intent[17..];
                let mut fields = content.split(|byte| *byte == 0);
                let name = Path::new(std::ffi::OsStr::from_bytes(fields.next().unwrap()));
                if name.components().count() != 1
                    || !matches!(
                        name.components().next(),
                        Some(std::path::Component::Normal(_))
                    )
                    || name == Path::new(DIRECTORY)
                {
                    return Err(std::io::Error::other(
                        "invalid recovery replacement filename",
                    ));
                }
                let entry = store.join(&id);
                if !std::fs::symlink_metadata(&entry)?.is_dir() {
                    return Err(std::io::Error::other("recovery entry must be a directory"));
                }
                let handle = entry.join(name);
                let mut version = None;
                let archived = match kind {
                    b'R' | b'D' => handle.clone(),
                    b'M' => {
                        let relative =
                            Path::new(std::ffi::OsStr::from_bytes(fields.next().ok_or_else(
                                || std::io::Error::other("missing recovery move target"),
                            )?));
                        if !normal_path(relative) {
                            return Err(std::io::Error::other("invalid recovery move target"));
                        }
                        let identity = fields.next().ok_or_else(|| {
                            std::io::Error::other("missing recovery mapping version")
                        })?;
                        match identity.split_first() {
                            Some((b'1', bytes)) => {
                                version = Some(std::str::from_utf8(bytes).map_err(|source| {
                                    std::io::Error::new(std::io::ErrorKind::InvalidData, source)
                                })?);
                            }
                            Some((b'0', [])) => {}
                            _ => {
                                return Err(std::io::Error::other(
                                    "invalid recovery mapping version",
                                ))
                            }
                        }
                        root.join(relative)
                    }
                    _ => return Err(std::io::Error::other("unknown recovery intent")),
                };
                if fields.next().is_some() {
                    return Err(std::io::Error::other("invalid recovery intent"));
                }
                let destination = directory.join(name);
                if kind != b'R'
                    && ((kind == b'M' && version.is_none()) || !tracked(&destination, version)?)
                {
                    complete_replacement(&handle)?;
                    continue;
                }
                if std::fs::symlink_metadata(&destination).is_ok() {
                    complete_replacement(&handle)?;
                    continue;
                }
                let metadata = std::fs::symlink_metadata(&archived)?;
                if metadata.dev() != device || metadata.ino() != inode {
                    return Err(std::io::Error::other("recovery source identity changed"));
                }
                match rename_no_replace(&archived, &destination) {
                    Ok(()) => {
                        std::fs::File::open(archived.parent().unwrap())?.sync_all()?;
                        std::fs::File::open(&directory)?.sync_all()?;
                        directories.push(destination.parent().unwrap().to_path_buf());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error)
                        if error.kind() == std::io::ErrorKind::NotFound
                            && std::fs::symlink_metadata(&destination).is_ok() => {}
                    Err(error) => return Err(error),
                }
                complete_replacement(&handle)?;
            }
        }
    }
    Ok(())
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
    fn interrupted_replacements_never_overwrite_an_already_published_or_new_local_path() {
        let root = std::env::temp_dir().join(format!("gdrive-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("file");
        std::fs::write(&path, b"old").unwrap();
        let archived = archive_replacement(&path).unwrap().unwrap();
        std::fs::write(&path, b"new").unwrap();
        restore_interrupted(&root, |_, _| Ok(true)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::read(&archived).unwrap(), b"old");
        assert_eq!(
            std::fs::read_dir(root.join(DIRECTORY).join("pending"))
                .unwrap()
                .count(),
            0
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restored_directory_moves_skip_already_queued_vanished_descendants() {
        let root = std::env::temp_dir().join(format!("gdrive-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let original = root.join("original");
        let moved = root.join("moved");
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("child"), b"keep").unwrap();
        move_with_intent(&root, &original, &moved, Some("version")).unwrap();
        restore_queued(&root, vec![moved.clone(), root.clone()], |_, _| Ok(true)).unwrap();
        assert_eq!(std::fs::read(original.join("child")).unwrap(), b"keep");
        assert!(!moved.exists());
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            restore_interrupted(&root, |_, _| Ok(true))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn interrupted_moves_do_not_restore_a_replaced_unrelated_inode() {
        let root = std::env::temp_dir().join(format!("gdrive-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let original = root.join("original");
        let moved = root.join("moved");
        std::fs::write(&original, b"old").unwrap();
        move_with_intent(&root, &original, &moved, Some("version")).unwrap();
        std::fs::rename(&moved, root.join("kept")).unwrap();
        std::fs::write(&moved, b"unrelated").unwrap();
        assert!(restore_interrupted(&root, |_, _| Ok(true)).is_err());
        assert!(!original.exists());
        assert_eq!(std::fs::read(moved).unwrap(), b"unrelated");
        assert_eq!(std::fs::read(root.join("kept")).unwrap(), b"old");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fifo_markers_are_rejected_without_waiting_for_a_writer() {
        let root = std::env::temp_dir().join(format!("gdrive-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let marker = CString::new(root.join("owner").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { nix::libc::mkfifo(marker.as_ptr(), 0o600) }, 0);
        let (sender, receiver) = std::sync::mpsc::channel();
        let directory = root.clone();
        let worker = std::thread::spawn(move || {
            sender.send(validate(&directory)).unwrap();
        });
        assert!(receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
            .is_err());
        worker.join().unwrap();
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
