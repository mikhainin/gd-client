//! Applies the configured owning user/group to synchronised files on disk.

use std::path::Path;

use nix::unistd::{Gid, Group, Uid, User};

use crate::error::SyncError;

/// Resolves a configured user/group name pair to numeric uid/gid, falling
/// back to the current process's effective uid/gid ("current user") when
/// the name is empty or cannot be resolved, so a misconfiguration never
/// blocks sync - only ownership.
pub fn resolve_owner(owner_user: &str, owner_group: &str) -> (Uid, Gid) {
    let uid = if owner_user.is_empty() {
        None
    } else {
        User::from_name(owner_user).ok().flatten().map(|u| u.uid)
    }
    .unwrap_or_else(Uid::current);

    let gid = if owner_group.is_empty() {
        None
    } else {
        Group::from_name(owner_group).ok().flatten().map(|g| g.gid)
    }
    .unwrap_or_else(Gid::current);

    (uid, gid)
}

/// Applies ownership to a single path. Errors are logged by the caller
/// rather than aborting sync - not being able to `chown` (e.g. when the
/// service isn't running as root) shouldn't stop files from being synced.
pub fn apply_ownership(path: &Path, uid: Uid, gid: Gid) -> Result<(), SyncError> {
    std::os::unix::fs::chown(path, Some(uid.as_raw()), Some(gid.as_raw()))?;
    Ok(())
}
