use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::local_fs::LocalFs;

pub(crate) const DIRECTORY: &str = ".gdrive-recovery";
pub(crate) const INITIALIZING_PREFIX: &str = ".gdrive-recovery-init-";
const MARKER: &str = "g-client local recovery v2\n";

pub(crate) struct Recovery<'a> {
    fs: LocalFs<'a>,
    owner: uuid::Uuid,
}

impl<'a> Recovery<'a> {
    pub(crate) fn new(root: &'a Path, owner: uuid::Uuid) -> Self {
        Self {
            fs: LocalFs(root),
            owner,
        }
    }

    fn marker(&self) -> String {
        format!("{MARKER}{}\n", self.owner)
    }

    pub(crate) fn validate(&self, directory: &Path) -> std::io::Result<()> {
        let metadata = self.fs.metadata(directory)?;
        let marker = self.marker();
        if !metadata.is_dir()
            || self
                .fs
                .read_limited(&directory.join("owner"), marker.len() as u64)?
                != marker.as_bytes()
        {
            return Err(std::io::Error::other(
                "reserved .gdrive-recovery path is not owned by this synchronization folder",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn archive(&self, path: &Path) -> std::io::Result<Option<PathBuf>> {
        self.archive_path(path, None)
    }

    pub(crate) fn archive_replacement(&self, path: &Path) -> std::io::Result<Option<PathBuf>> {
        self.archive_path(path, Some(b'R'))
    }

    pub(crate) fn archive_removal(&self, path: &Path) -> std::io::Result<Option<PathBuf>> {
        self.archive_path(path, Some(b'D'))
    }

    fn create_entry(&self, path: &Path) -> std::io::Result<PathBuf> {
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("recovery path has no parent"))?;
        let directory =
            self.initialize_store(parent, |marker| marker.write_all(self.marker().as_bytes()))?;
        let entry = directory.join(uuid::Uuid::new_v4().to_string());
        self.fs.create_private_dir(&entry)?;
        self.fs.sync_dir(&directory)?;
        Ok(entry)
    }

    fn initialize_store(
        &self,
        parent: &Path,
        write_marker: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
    ) -> std::io::Result<PathBuf> {
        let directory = parent.join(DIRECTORY);
        match self.fs.metadata(&directory) {
            Ok(_) => {
                self.validate(&directory)?;
                return Ok(directory);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let temporary = parent.join(format!("{INITIALIZING_PREFIX}{}", uuid::Uuid::new_v4()));
        self.fs.create_private_dir(&temporary)?;
        let result = (|| {
            let mut marker = self.fs.create_new(&temporary.join("owner"))?;
            write_marker(&mut marker)?;
            marker.sync_all()?;
            drop(marker);
            self.fs.sync_dir(&temporary)?;
            match self.fs.rename_no_replace(&temporary, &directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    self.validate(&directory)?
                }
                Err(error) => return Err(error),
            }
            self.fs.sync_dir(parent)?;
            Ok(directory)
        })();
        if let Err(error) = self.fs.remove_dir_all(&temporary) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%error, "could not remove recovery initialization staging directory");
            }
        }
        result
    }

    fn write_intent(
        &self,
        entry: &Path,
        path: &Path,
        kind: u8,
        target: Option<&Path>,
        version: Option<&str>,
    ) -> std::io::Result<()> {
        let directory = entry.parent().unwrap();
        let pending = directory.join("pending");
        match self.fs.create_private_dir(&pending) {
            Ok(()) => {
                self.fs.sync_dir(directory)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if !self.fs.metadata(&pending)?.is_dir() {
                    return Err(std::io::Error::other(
                        "recovery pending path must be a directory",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
        let temporary = entry.join(".intent");
        let mut file = self.fs.create_new(&temporary)?;
        let metadata = self.fs.metadata(path)?;
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
        self.fs
            .rename_no_replace(&temporary, &pending.join(entry.file_name().unwrap()))?;
        self.fs.sync_dir(&pending)
    }

    // The returned handle identifies the journal; the data itself remains at `to`.
    pub(crate) fn move_with_intent(
        &self,
        from: &Path,
        to: &Path,
        version: Option<&str>,
    ) -> std::io::Result<PathBuf> {
        let relative = to
            .strip_prefix(self.fs.0)
            .map_err(|source| std::io::Error::new(std::io::ErrorKind::InvalidInput, source))?;
        if !normal_path(relative) {
            return Err(std::io::Error::other("invalid recovery move target"));
        }
        let entry = self.create_entry(from)?;
        self.write_intent(&entry, from, b'M', Some(relative), version)?;
        if let Err(error) = self.fs.rename_no_replace(from, to) {
            self.complete_replacement(&entry.join(from.file_name().unwrap()))?;
            return Err(error);
        }
        self.fs.sync_dir(from.parent().unwrap())?;
        self.fs.sync_dir(to.parent().unwrap())?;
        Ok(entry.join(from.file_name().unwrap()))
    }

    fn archive_path(&self, path: &Path, intent: Option<u8>) -> std::io::Result<Option<PathBuf>> {
        match self.fs.metadata(path) {
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
        let entry = self.create_entry(path)?;
        let destination = entry.join(
            path.file_name()
                .ok_or_else(|| std::io::Error::other("recovery path has no filename"))?,
        );
        if let Some(kind) = intent {
            self.write_intent(&entry, path, kind, None, None)?;
        }
        match self.fs.rename_no_replace(path, &destination) {
            Ok(()) => {
                self.fs.sync_dir(&entry)?;
                self.fs.sync_dir(parent)?;
                Ok(Some(destination))
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                if intent.is_some() {
                    self.complete_replacement(&destination)?;
                }
                self.fs.remove_dir(&entry)?;
                Ok(None)
            }
            Err(error) => {
                if intent.is_some() {
                    self.complete_replacement(&destination)?;
                }
                if let Err(cleanup) = self.fs.remove_dir(&entry) {
                    tracing::warn!(%cleanup, "could not remove empty recovery entry");
                }
                Err(error)
            }
        }
    }

    pub(crate) fn complete_replacement(&self, archived: &Path) -> std::io::Result<()> {
        let entry = archived
            .parent()
            .ok_or_else(|| std::io::Error::other("invalid recovery entry"))?;
        let directory = entry
            .parent()
            .ok_or_else(|| std::io::Error::other("invalid recovery store"))?;
        let pending = directory.join("pending");
        self.validate(directory)?;
        match self
            .fs
            .remove_file(&pending.join(entry.file_name().unwrap()))
        {
            Ok(()) => self.fs.sync_dir(&pending),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn restore_interrupted(
        &self,
        tracked: impl Fn(&Path, Option<&str>) -> std::io::Result<bool>,
    ) -> std::io::Result<()> {
        self.restore_queued(vec![self.fs.0.to_path_buf()], tracked)
    }

    fn restore_queued(
        &self,
        mut directories: Vec<PathBuf>,
        tracked: impl Fn(&Path, Option<&str>) -> std::io::Result<bool>,
    ) -> std::io::Result<()> {
        while let Some(directory) = directories.pop() {
            let items = match self.fs.read_dir(&directory) {
                Ok(items) => items,
                Err(error)
                    if directory != self.fs.0
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
                let (name, file_type) = item?;
                if !file_type.is_dir() {
                    continue;
                }
                if name.to_str().is_some_and(|name| {
                    name.starts_with(INITIALIZING_PREFIX) || name.starts_with(".gdrive-download-")
                }) {
                    continue;
                }
                if name != DIRECTORY {
                    directories.push(directory.join(name));
                    continue;
                }
                let store = directory.join(name);
                self.validate(&store)?;
                let pending = store.join("pending");
                match self.fs.metadata(&pending) {
                    Ok(metadata) if metadata.is_dir() => {}
                    Ok(_) => {
                        return Err(std::io::Error::other(
                            "recovery pending path must be a directory",
                        ))
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                }
                for journal in self.fs.read_dir(&pending)? {
                    let (id, _) = journal?;
                    let _id = uuid::Uuid::parse_str(id.to_str().unwrap_or_default()).map_err(
                        |source| std::io::Error::new(std::io::ErrorKind::InvalidData, source),
                    )?;
                    let intent = self.fs.read_limited(&pending.join(&id), 8192)?;
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
                    if !self.fs.metadata(&entry)?.is_dir() {
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
                                    version =
                                        Some(std::str::from_utf8(bytes).map_err(|source| {
                                            std::io::Error::new(
                                                std::io::ErrorKind::InvalidData,
                                                source,
                                            )
                                        })?);
                                }
                                Some((b'0', [])) => {}
                                _ => {
                                    return Err(std::io::Error::other(
                                        "invalid recovery mapping version",
                                    ))
                                }
                            }
                            self.fs.0.join(relative)
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
                        self.complete_replacement(&handle)?;
                        continue;
                    }
                    if self.fs.try_exists(&destination)? {
                        self.complete_replacement(&handle)?;
                        continue;
                    }
                    let metadata = self.fs.metadata(&archived)?;
                    if metadata.dev() != device || metadata.ino() != inode {
                        return Err(std::io::Error::other("recovery source identity changed"));
                    }
                    match self.fs.rename_no_replace(&archived, &destination) {
                        Ok(()) => {
                            self.fs.sync_dir(archived.parent().unwrap())?;
                            self.fs.sync_dir(&directory)?;
                            directories.push(destination.parent().unwrap().to_path_buf());
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(error)
                            if error.kind() == std::io::ErrorKind::NotFound
                                && self.fs.try_exists(&destination)? => {}
                        Err(error) => return Err(error),
                    }
                    self.complete_replacement(&handle)?;
                }
            }
        }
        Ok(())
    }
}

fn normal_path(path: &Path) -> bool {
    path.components().next().is_some() && path.components().all(|component|
        matches!(component, std::path::Component::Normal(name) if name != DIRECTORY))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn moved_journals_cannot_be_consumed_by_another_binding() {
        let temporary =
            std::env::temp_dir().join(format!("gdrive-recovery-{}", uuid::Uuid::new_v4()));
        let first = temporary.join("first");
        let second = temporary.join("second");
        std::fs::create_dir_all(first.join("sub")).unwrap();
        std::fs::create_dir(&second).unwrap();
        let source = first.join("sub/file");
        std::fs::write(&source, b"original").unwrap();
        let recovery = Recovery::new(&first, uuid::Uuid::new_v4());
        recovery
            .move_with_intent(&source, &first.join("target"), Some("version"))
            .unwrap();
        std::fs::rename(first.join("sub"), second.join("sub")).unwrap();
        std::fs::write(second.join("target"), b"unrelated").unwrap();
        let foreign = Recovery::new(&second, uuid::Uuid::new_v4());
        assert!(foreign
            .restore_interrupted(|_, _| panic!("foreign journal reached state lookup"))
            .is_err());
        assert_eq!(std::fs::read(first.join("target")).unwrap(), b"original");
        assert_eq!(std::fs::read(second.join("target")).unwrap(), b"unrelated");
        assert_eq!(
            std::fs::read_dir(second.join("sub").join(DIRECTORY).join("pending"))
                .unwrap()
                .count(),
            1
        );
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn descendant_aliases_fail_without_clearing_recovery_intents() {
        let temporary =
            std::env::temp_dir().join(format!("gdrive-recovery-{}", uuid::Uuid::new_v4()));
        let root = temporary.join("root");
        let other = temporary.join("other");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("sentinel"), b"keep").unwrap();
        std::os::unix::fs::symlink(&other, root.join("sub")).unwrap();
        let recovery = Recovery::new(&root, uuid::Uuid::new_v4());
        assert!(recovery
            .archive_replacement(&root.join("sub/sentinel"))
            .is_err());
        assert!(!other.join(DIRECTORY).exists());
        let path = root.join("file");
        std::fs::write(&path, b"original").unwrap();
        let archived = recovery.archive_replacement(&path).unwrap().unwrap();
        std::os::unix::fs::symlink(other.join("sentinel"), &path).unwrap();
        assert!(recovery.restore_interrupted(|_, _| Ok(true)).is_err());
        assert_eq!(std::fs::read(archived).unwrap(), b"original");
        assert_eq!(std::fs::read(other.join("sentinel")).unwrap(), b"keep");
        assert_eq!(
            std::fs::read_dir(root.join(DIRECTORY).join("pending"))
                .unwrap()
                .count(),
            1
        );
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn failed_or_interrupted_store_initialization_never_publishes_a_partial_marker() {
        let root = std::env::temp_dir().join(format!("gdrive-recovery-{}", uuid::Uuid::new_v4()));
        let recovery = Recovery::new(&root, uuid::Uuid::nil());
        std::fs::create_dir(&root).unwrap();
        let error = recovery
            .initialize_store(&root, |marker| {
                marker.write_all(b"partial")?;
                Err(std::io::Error::from(std::io::ErrorKind::StorageFull))
            })
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::StorageFull);
        assert!(!root.join(DIRECTORY).exists());
        let interrupted = root.join(format!("{INITIALIZING_PREFIX}{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&interrupted).unwrap();
        std::fs::write(interrupted.join("owner"), b"partial").unwrap();
        let store = recovery
            .initialize_store(&root, |marker| {
                marker.write_all(recovery.marker().as_bytes())
            })
            .unwrap();
        recovery.validate(&store).unwrap();
        recovery.restore_interrupted(|_, _| Ok(false)).unwrap();
        assert_eq!(
            std::fs::read(interrupted.join("owner")).unwrap(),
            b"partial"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn archived_files_and_directories_retain_writes_through_open_handles() {
        let root = std::env::temp_dir().join(format!("gdrive-recovery-{}", uuid::Uuid::new_v4()));
        let recovery = Recovery::new(&root, uuid::Uuid::nil());
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
            let archived = recovery.archive(&path).unwrap().unwrap();
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
        let recovery = Recovery::new(&root, uuid::Uuid::nil());
        std::fs::create_dir(&root).unwrap();
        let path = root.join("file");
        std::fs::write(&path, b"old").unwrap();
        let archived = recovery.archive_replacement(&path).unwrap().unwrap();
        std::fs::write(&path, b"new").unwrap();
        recovery.restore_interrupted(|_, _| Ok(true)).unwrap();
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
        let recovery = Recovery::new(&root, uuid::Uuid::nil());
        std::fs::create_dir(&root).unwrap();
        let original = root.join("original");
        let moved = root.join("moved");
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("child"), b"keep").unwrap();
        recovery
            .move_with_intent(&original, &moved, Some("version"))
            .unwrap();
        recovery
            .restore_queued(vec![moved.clone(), root.clone()], |_, _| Ok(true))
            .unwrap();
        assert_eq!(std::fs::read(original.join("child")).unwrap(), b"keep");
        assert!(!moved.exists());
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            recovery
                .restore_interrupted(|_, _| Ok(true))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn interrupted_moves_do_not_restore_a_replaced_unrelated_inode() {
        let root = std::env::temp_dir().join(format!("gdrive-recovery-{}", uuid::Uuid::new_v4()));
        let recovery = Recovery::new(&root, uuid::Uuid::nil());
        std::fs::create_dir(&root).unwrap();
        let original = root.join("original");
        let moved = root.join("moved");
        std::fs::write(&original, b"old").unwrap();
        recovery
            .move_with_intent(&original, &moved, Some("version"))
            .unwrap();
        std::fs::rename(&moved, root.join("kept")).unwrap();
        std::fs::write(&moved, b"unrelated").unwrap();
        assert!(recovery.restore_interrupted(|_, _| Ok(true)).is_err());
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
            sender
                .send(Recovery::new(&directory, uuid::Uuid::nil()).validate(&directory))
                .unwrap();
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
        let recovery = Recovery::new(&root, uuid::Uuid::nil());
        std::fs::create_dir(&root).unwrap();
        let path = root.join("file");
        std::fs::write(&path, b"keep").unwrap();
        let reserved = root.join(DIRECTORY);
        std::fs::create_dir(&reserved).unwrap();
        assert!(recovery.archive(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
        std::fs::remove_dir(&reserved).unwrap();
        std::os::unix::fs::symlink(&root, &reserved).unwrap();
        assert!(recovery.archive(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
        std::fs::remove_dir_all(root).unwrap();
    }
}
