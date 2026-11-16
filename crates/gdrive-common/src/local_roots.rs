//! Resolve local roots without creating them, and reject overlapping active bindings.

use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use uuid::Uuid;

use crate::{CommonError, SyncFolder};

mod case_sensitivity;
mod mounts;
pub use case_sensitivity::{verify_case_sensitive_directory, verify_local_filesystem};

#[derive(Clone)]
pub struct ResolvedRoot {
    pub path: PathBuf,
    backing: Vec<mounts::BackingRoot>,
}

impl ResolvedRoot {
    pub fn new(path: &Path) -> Result<Self, CommonError> {
        let path = resolve(path)?;
        let backing = mounts::footprints(&path).map_err(|source| CommonError::LocalRoot {
            path: path.clone(),
            source,
        })?;
        Ok(Self { path, backing })
    }

    pub fn overlaps(&self, other: &Self) -> bool {
        self.backing.iter().any(|first| {
            other
                .backing
                .iter()
                .any(|second| first.device == second.device && overlap(&first.path, &second.path))
        })
    }
}

/// Canonicalize existing components, preserving a normalized not-yet-created suffix.
/// Dangling symlinks and uninspectable components fail closed.
pub fn resolve(path: &Path) -> Result<PathBuf, CommonError> {
    resolve_path(path).map_err(|source| CommonError::LocalRoot {
        path: path.to_path_buf(),
        source,
    })
}

fn resolve_path(path: &Path) -> std::io::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "local sync root is empty",
        ));
    }
    let mut resolved = PathBuf::new();
    for component in std::path::absolute(path)?.components() {
        if component == Component::ParentDir {
            resolved.pop();
            continue;
        }
        resolved.push(component.as_os_str());
        match std::fs::canonicalize(&resolved) {
            Ok(canonical) => {
                if !std::fs::metadata(&canonical)?.is_dir() {
                    return Err(std::io::ErrorKind::NotADirectory.into());
                }
                resolved = canonical;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(&resolved) {
                    Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => {}
                    Ok(_) => return Err(error),
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
    inspect_ancestors(&resolved, |_, directory| {
        verify_case_sensitive_directory(directory)
    })?;
    Ok(resolved)
}

fn inspect_ancestors(
    path: &Path,
    mut inspect: impl FnMut(&Path, &std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    for ancestor in path.ancestors() {
        let directory = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(ancestor)
        {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        inspect(ancestor, &directory)?;
    }
    Ok(())
}

/// Compare directory components, not string prefixes such as `photos`/`photos-old`.
fn overlap(first: &Path, second: &Path) -> bool {
    first.starts_with(second) || second.starts_with(first)
}

/// Return every affected enabled binding, leaving unrelated roots runnable.
pub fn errors(folders: &[SyncFolder]) -> Vec<(Uuid, CommonError)> {
    let mut seen = std::collections::HashSet::new();
    let duplicates: std::collections::HashSet<_> = folders
        .iter()
        .filter(|folder| folder.enabled)
        .filter_map(|folder| (!seen.insert(folder.id)).then_some(folder.id))
        .collect();
    let mut errors = Vec::new();
    let mut roots = Vec::new();
    for folder in folders.iter().filter(|folder| folder.enabled) {
        if duplicates.contains(&folder.id) {
            errors.push((
                folder.id,
                CommonError::LocalRoot {
                    path: folder.local_path.clone(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "duplicate enabled synchronization folder ID",
                    ),
                },
            ));
            continue;
        }
        match ResolvedRoot::new(&folder.local_path) {
            Ok(root) => roots.push((folder.id, root)),
            Err(error) => errors.push((folder.id, error)),
        }
    }
    for (index, (id, root)) in roots.iter().enumerate() {
        for (other_id, other) in &roots[index + 1..] {
            if root.overlaps(other) {
                let conflict = || CommonError::OverlappingRoots {
                    first: root.path.clone(),
                    second: other.path.clone(),
                };
                errors.extend([(*id, conflict()), (*other_id, conflict())]);
            }
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::os::unix::fs::symlink;

    #[test]
    fn canonical_targets_inspect_ancestors_even_with_a_missing_suffix() {
        let root = std::env::temp_dir().join(format!("gdrive-roots-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("target/parent/leaf")).unwrap();
        symlink(root.join("target/parent/leaf"), root.join("alias")).unwrap();
        let resolved = resolve(&root.join("alias/future")).unwrap();
        assert_eq!(resolved, root.join("target/parent/leaf/future"));
        let error = inspect_ancestors(&resolved, |ancestor, _| {
            if ancestor == root.join("target/parent") {
                Err(std::io::ErrorKind::PermissionDenied.into())
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(!root.join("target/parent/leaf/future").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn duplicate_enabled_ids_are_rejected_even_for_disjoint_paths() {
        let root = std::env::temp_dir().join(format!("gdrive-roots-{}", Uuid::new_v4()));
        let first = SyncFolder::new("one", root.join("first"), "", "");
        let mut second = SyncFolder::new("two", root.join("second"), "", "");
        second.id = first.id;
        let mut folders = [first, second];
        let issues = errors(&folders);
        assert_eq!(issues.len(), 2);
        assert!(issues
            .iter()
            .all(|(_, error)| error.to_string().contains("duplicate enabled")));
        assert!(!root.exists());
        folders[0].enabled = false;
        assert!(errors(&folders).is_empty());
        assert!(!root.exists());
    }

    #[test]
    fn equal_nested_and_symlinked_roots_conflict_without_creating_directories() {
        let root = std::env::temp_dir().join(format!("gdrive-roots-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("real/child")).unwrap();
        symlink(root.join("real/child"), root.join("alias")).unwrap();
        for (first, second) in [
            ("new", "new/child"),
            ("new/../real", "real"),
            ("alias/..", "real"),
            ("alias/future", "real/child/future/nested"),
        ] {
            let folders = [
                SyncFolder::new("one", root.join(first), "", ""),
                SyncFolder::new("two", root.join(second), "", ""),
                SyncFolder::new("sibling", root.join("real-sibling"), "", ""),
            ];
            let issues = errors(&folders);
            assert_eq!(issues.len(), 2, "{first}, {second}: {issues:?}");
            assert_eq!(issues[0].0, folders[0].id);
            assert_eq!(issues[1].0, folders[1].id);
        }
        assert!(!root.join("new").exists());
        assert!(!root.join("real/child/future").exists());
        let mut folders = [
            SyncFolder::new("one", root.join("real"), "", ""),
            SyncFolder::new("paused", root.join("alias"), "", ""),
        ];
        folders[1].enabled = false;
        assert!(errors(&folders).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dangling_symlinks_and_non_directories_fail_with_an_error_source() {
        let root = std::env::temp_dir().join(format!("gdrive-roots-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        symlink(root.join("absent"), root.join("dangling")).unwrap();
        std::fs::write(root.join("file"), b"keep").unwrap();
        for name in ["dangling/child", "file/../other", "file"] {
            let error = resolve(&root.join(name)).unwrap_err();
            assert!(error.source().is_some(), "{name}");
        }
        assert!(resolve(Path::new("")).is_err());
        assert!(!root.join("absent").exists());
        assert_eq!(std::fs::read(root.join("file")).unwrap(), b"keep");
        std::fs::remove_dir_all(root).unwrap();
    }
}
