use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

const CASEFOLD: u32 = 0x4000_0000;

/// Verify directory lookup semantics without creating probe files.
pub fn verify_case_sensitive_directory(directory: &File) -> io::Result<()> {
    let kind = verified_filesystem(directory)?;
    let mut flags: u32 = 0;
    let attributes = if unsafe {
        libc::ioctl(
            directory.as_raw_fd(),
            libc::FS_IOC_GETFLAGS,
            std::ptr::addr_of_mut!(flags),
        )
    } == 0
    {
        Ok(flags)
    } else {
        Err(io::Error::last_os_error())
    };
    verify_flags(kind, attributes)
}

pub fn verify_local_filesystem(file: &File) -> io::Result<()> {
    verified_filesystem(file).map(|_| ())
}

fn verified_filesystem(file: &File) -> io::Result<u32> {
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    if unsafe { libc::fstatfs(file.as_raw_fd(), filesystem.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let kind = unsafe { filesystem.assume_init() }.f_type as u32;
    if !supported(kind) {
        return Err(unsupported("local filesystem is not verified case-sensitive; use non-casefolded ext4, Btrfs, or tmpfs"));
    }
    Ok(kind)
}

fn supported(kind: u32) -> bool {
    [
        libc::EXT4_SUPER_MAGIC,
        libc::BTRFS_SUPER_MAGIC,
        libc::TMPFS_MAGIC,
    ]
    .iter()
    .any(|magic| *magic as u32 == kind)
}

fn verify_flags(kind: u32, attributes: io::Result<u32>) -> io::Result<()> {
    match attributes {
        Ok(flags) if flags & CASEFOLD != 0 => Err(unsupported(
            "case-insensitive local directories are not supported",
        )),
        Ok(_) => Ok(()),
        // Older tmpfs kernels have neither casefolding nor the attribute ioctl.
        Err(error)
            if kind == libc::TMPFS_MAGIC as u32 && error.raw_os_error() == Some(libc::ENOTTY) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn unsupported(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn casefold_and_unverified_filesystems_fail_closed() {
        assert!(!supported(0x4d44)); // FAT
        assert!(!supported(0x2011_bab0)); // exFAT
        for kind in [
            libc::EXT4_SUPER_MAGIC,
            libc::BTRFS_SUPER_MAGIC,
            libc::TMPFS_MAGIC,
        ] {
            assert!(supported(kind as u32));
            assert!(verify_flags(kind as u32, Ok(CASEFOLD)).is_err());
            assert!(verify_flags(kind as u32, Ok(0)).is_ok());
            let error = verify_flags(kind as u32, Err(io::Error::from_raw_os_error(libc::EACCES)))
                .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EACCES));
        }
        assert!(verify_flags(
            libc::TMPFS_MAGIC as u32,
            Err(io::Error::from_raw_os_error(libc::ENOTTY))
        )
        .is_ok());
        assert!(verify_flags(
            libc::EXT4_SUPER_MAGIC as u32,
            Err(io::Error::from_raw_os_error(libc::ENOTTY))
        )
        .is_err());
    }
}
