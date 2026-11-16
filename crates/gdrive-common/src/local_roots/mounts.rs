use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub(super) struct BackingRoot {
    pub device: String,
    pub path: PathBuf,
}

struct Mount {
    id: u64,
    device: String,
    root: PathBuf,
    point: PathBuf,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn decode(value: &[u8]) -> io::Result<PathBuf> {
    let mut decoded = Vec::new();
    let mut bytes = value.iter().copied();
    while let Some(byte) = bytes.next() {
        if byte == b'\\' {
            let digits: Vec<_> = bytes.by_ref().take(3).collect();
            if digits.len() != 3 {
                return Err(invalid("invalid escaped mount path"));
            }
            let digits = std::str::from_utf8(&digits)
                .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))?;
            decoded.push(
                u8::from_str_radix(digits, 8)
                    .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))?,
            );
        } else {
            decoded.push(byte);
        }
    }
    Ok(PathBuf::from(std::ffi::OsString::from_vec(decoded)))
}

fn parse(text: &[u8]) -> io::Result<Vec<Mount>> {
    text.split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let fields: Vec<_> = line
                .split(|byte| *byte == b' ')
                .filter(|field| !field.is_empty())
                .collect();
            if fields.len() < 10 || !fields[6..].contains(&b"-".as_slice()) {
                return Err(invalid("invalid Linux mount information"));
            }
            let point = decode(fields[4])?;
            if !point.is_absolute() {
                return Err(invalid("mount points must be absolute"));
            }
            Ok(Mount {
                id: std::str::from_utf8(fields[0])
                    .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))?
                    .parse()
                    .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))?,
                device: std::str::from_utf8(fields[2])
                    .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))?
                    .to_owned(),
                root: decode(fields[3])?,
                point,
            })
        })
        .collect()
}

fn visible_mount(path: &Path) -> io::Result<u64> {
    let mut ancestor = path;
    let file = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
            .open(ancestor)
        {
            Ok(file) => break file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                ancestor = ancestor.parent().ok_or(error)?;
            }
            Err(error) => return Err(error),
        }
    };
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .ok_or_else(|| invalid("directory descriptor has no Linux mount ID"))?
        .trim()
        .parse()
        .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))
}

fn from_mounts(
    path: &Path,
    mounts: &[Mount],
    visible: impl Fn(&Path) -> io::Result<u64>,
) -> io::Result<Vec<BackingRoot>> {
    let id = visible(path)?;
    let mount = mounts
        .iter()
        .find(|mount| mount.id == id)
        .ok_or_else(|| invalid("mount topology changed while resolving sync root; retry"))?;
    let relative = path
        .strip_prefix(&mount.point)
        .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))?;
    let mut backing = vec![BackingRoot {
        device: mount.device.clone(),
        path: mount.root.join(relative),
    }];
    let mut nested_mounts: Vec<_> = mounts
        .iter()
        .filter(|mount| mount.point != path && mount.point.starts_with(path))
        .collect();
    nested_mounts.sort_by_key(|mount| mount.point.components().count());
    let mut visible_parents = vec![mount];
    for nested in nested_mounts {
        match visible(&nested.point) {
            Ok(id) if id == nested.id => {
                let parent = visible_parents
                    .iter()
                    .filter(|parent| nested.point.starts_with(&parent.point))
                    .max_by_key(|parent| parent.point.components().count())
                    .unwrap();
                let relative = nested.point.strip_prefix(&parent.point).unwrap();
                if nested.device != parent.device || nested.root != parent.root.join(relative) {
                    backing.push(BackingRoot {
                        device: nested.device.clone(),
                        path: nested.root.clone(),
                    });
                }
                visible_parents.push(nested);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    for (index, first) in backing.iter().enumerate() {
        if backing[index + 1..].iter().any(|second| {
            first.device == second.device && super::overlap(&first.path, &second.path)
        }) {
            return Err(invalid(
                "local sync root contains overlapping mount aliases",
            ));
        }
    }
    Ok(backing)
}

pub(super) fn footprints(path: &Path) -> io::Result<Vec<BackingRoot>> {
    let mounts = parse(&std::fs::read("/proc/self/mountinfo")?)?;
    from_mounts(path, &mounts, visible_mount)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_roots::ResolvedRoot;

    #[test]
    fn self_aliased_mount_trees_are_rejected_before_recursive_traversal() {
        for (root, nested, expected) in [
            ("/data", "/data", false),
            ("/data", "/data/child", false),
            ("/data", "/", false),
            ("/data", "/elsewhere", true),
            ("/data", "/data/mounted", true),
        ] {
            let mounts = parse(
                format!(
                    "1 0 8:1 / / rw - ext4 /dev/fixture rw\n\
                 2 1 8:1 {nested} {root}/mounted rw - ext4 /dev/fixture rw\n"
                )
                .as_bytes(),
            )
            .unwrap();
            let result = from_mounts(Path::new(root), &mounts, |candidate| {
                Ok(if candidate == Path::new(root) { 1 } else { 2 })
            });
            assert_eq!(result.is_ok(), expected, "nested source {nested}");
        }
        let mounts = parse(
            b"1 0 8:1 / / rw - ext4 /dev/fixture rw\n\
            2 1 8:1 /external /data/first rw - ext4 /dev/fixture rw\n\
            3 1 8:1 /external /data/second rw - ext4 /dev/fixture rw\n",
        )
        .unwrap();
        assert!(from_mounts(Path::new("/data"), &mounts, |candidate| {
            Ok(match candidate.to_str().unwrap() {
                "/data/first" => 2,
                "/data/second" => 3,
                _ => 1,
            })
        })
        .is_err());
        let mounts = parse(
            b"1 0 8:1 / / rw - ext4 /dev/fixture rw\n\
            2 1 8:2 /external /data/first rw - ext4 /dev/other rw\n\
            3 2 8:2 /external/child /data/first/child rw - ext4 /dev/other rw\n",
        )
        .unwrap();
        assert!(from_mounts(Path::new("/data"), &mounts, |candidate| {
            Ok(match candidate.to_str().unwrap() {
                "/data/first" => 2,
                "/data/first/child" => 3,
                _ => 1,
            })
        })
        .is_ok());
    }

    #[test]
    fn fifo_mount_inspection_does_not_open_it_for_io() {
        let directory = std::env::temp_dir().join(format!("gdrive-fifo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let fifo = directory.join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (sender, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn({
            let fifo = fifo.clone();
            move || sender.send(visible_mount(&fifo)).unwrap()
        });
        let result = received.recv_timeout(std::time::Duration::from_secs(1));
        let rescue = if result.is_err() {
            Some(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&fifo)
                    .unwrap(),
            )
        } else {
            None
        };
        worker.join().unwrap();
        assert!(result.unwrap().is_ok());
        drop(rescue);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bind_aliases_and_nested_mounts_share_backing_ancestry() {
        let mounts = parse(
            concat!(
                "1 0 8:1 / / rw - ext4 /dev/fixture rw\n",
                "2 1 8:1 /data/subtree /mirror rw - ext4 /dev/fixture rw\n",
                "3 1 8:1 /data/other /elsewhere/mounted rw - ext4 /dev/fixture rw\n",
                "4 1 8:2 / /independent rw - ext4 /dev/other rw\n",
            )
            .as_bytes(),
        )
        .unwrap();
        let root = |path: &str| {
            let path = PathBuf::from(path);
            let backing = from_mounts(&path, &mounts, |candidate| {
                Ok(mounts
                    .iter()
                    .filter(|mount| candidate.starts_with(&mount.point))
                    .max_by_key(|mount| mount.point.components().count())
                    .unwrap()
                    .id)
            })
            .unwrap();
            ResolvedRoot { path, backing }
        };
        assert!(root("/data").overlaps(&root("/mirror/new")));
        assert!(root("/mirror/new").overlaps(&root("/data/subtree/new/child")));
        assert!(root("/data").overlaps(&root("/elsewhere")));
        assert!(!root("/mirror/new").overlaps(&root("/data/subtree/sibling")));
        assert!(!root("/data").overlaps(&root("/independent/data")));
    }

    #[test]
    fn escaped_mount_paths_decode_without_losing_component_boundaries() {
        let mounts = parse(b"1 0 8:1 /a\\040b /mount\\134name rw - ext4 /dev/fixture rw").unwrap();
        assert_eq!(mounts[0].root, Path::new("/a b"));
        assert_eq!(mounts[0].point, Path::new("/mount\\name"));
        assert!(parse(b"2 1 0:4 net:[123] /namespace rw - nsfs nsfs rw").is_ok());
        assert!(parse(b"1 0 8:1 /bad\\00 /point rw - ext4 /dev/fixture rw").is_err());
        let bytes =
            parse(b"1 0 8:1 /raw-\xff /mount-\xfe\r\x0c rw - ext4 /dev/fixture rw").unwrap();
        assert_eq!(
            bytes[0].root,
            PathBuf::from(std::ffi::OsString::from_vec(b"/raw-\xff".to_vec()))
        );
        assert_eq!(
            bytes[0].point,
            PathBuf::from(std::ffi::OsString::from_vec(b"/mount-\xfe\r\x0c".to_vec()))
        );
    }
}
