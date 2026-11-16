use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
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

fn decode(value: &str) -> io::Result<PathBuf> {
    let mut decoded = Vec::new();
    let mut bytes = value.bytes();
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

fn parse(text: &str) -> io::Result<Vec<Mount>> {
    text.lines()
        .map(|line| {
            let fields: Vec<_> = line.split_ascii_whitespace().collect();
            if fields.len() < 10 || !fields[6..].contains(&"-") {
                return Err(invalid("invalid Linux mount information"));
            }
            let point = decode(fields[4])?;
            if !point.is_absolute() {
                return Err(invalid("mount points must be absolute"));
            }
            Ok(Mount {
                id: fields[0]
                    .parse()
                    .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))?,
                device: fields[2].to_owned(),
                root: decode(fields[3])?,
                point,
            })
        })
        .collect()
}

fn visible_mount(path: &Path) -> io::Result<u64> {
    let mut ancestor = path;
    let file = loop {
        match std::fs::File::open(ancestor) {
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
    for nested in mounts
        .iter()
        .filter(|mount| mount.point != path && mount.point.starts_with(path))
    {
        match visible(&nested.point) {
            Ok(id) if id == nested.id => backing.push(BackingRoot {
                device: nested.device.clone(),
                path: nested.root.clone(),
            }),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(backing)
}

pub(super) fn footprints(path: &Path) -> io::Result<Vec<BackingRoot>> {
    let mounts = parse(&std::fs::read_to_string("/proc/self/mountinfo")?)?;
    from_mounts(path, &mounts, visible_mount)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_roots::ResolvedRoot;

    #[test]
    fn bind_aliases_and_nested_mounts_share_backing_ancestry() {
        let mounts = parse(concat!(
            "1 0 8:1 / / rw - ext4 /dev/fixture rw\n",
            "2 1 8:1 /data/subtree /mirror rw - ext4 /dev/fixture rw\n",
            "3 1 8:1 /data/other /elsewhere/mounted rw - ext4 /dev/fixture rw\n",
            "4 1 8:2 / /independent rw - ext4 /dev/other rw\n",
        ))
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
        let mounts = parse("1 0 8:1 /a\\040b /mount\\134name rw - ext4 /dev/fixture rw").unwrap();
        assert_eq!(mounts[0].root, Path::new("/a b"));
        assert_eq!(mounts[0].point, Path::new("/mount\\name"));
        assert!(parse("2 1 0:4 net:[123] /namespace rw - nsfs nsfs rw").is_ok());
        assert!(parse("1 0 8:1 /bad\\00 /point rw - ext4 /dev/fixture rw").is_err());
    }
}
