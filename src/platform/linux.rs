//! Linux backend: block-device ioctls, `O_DIRECT`, and mount probing via
//! procfs/sysfs.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::params::DEFAULT_SECTOR_SIZE;

/// nix-generated wrappers for the block-device ioctls we need.
mod ioctl {
    // BLKSSZGET — logical sector size, `_IO(0x12, 104)` == 0x1268, returns an int.
    nix::ioctl_read_bad!(blksszget, 0x1268, nix::libc::c_int);
}

/// Detect the size of a source: regular files report their length, block
/// devices report their capacity — both via seeking to the end. Non-seekable
/// sources (pipes, character devices) return `None`.
pub fn detect_size(file: &mut File) -> Option<u64> {
    file.seek(SeekFrom::End(0)).ok()
}

/// Detect the logical sector size of a source. Block devices report it via
/// `BLKSSZGET`; everything else (regular files, pipes, an ioctl failure) falls
/// back to [`DEFAULT_SECTOR_SIZE`].
pub fn detect_sector_size(file: &File) -> u64 {
    let is_block = file
        .metadata()
        .is_ok_and(|m| m.file_type().is_block_device());
    if !is_block {
        return DEFAULT_SECTOR_SIZE;
    }
    let mut size: nix::libc::c_int = 0;
    // SAFETY: BLKSSZGET writes a single c_int through the pointer; the fd is
    // valid for the duration of the call.
    match unsafe { ioctl::blksszget(file.as_raw_fd(), &raw mut size) } {
        Ok(_) => u64::try_from(size)
            .ok()
            .filter(|&s| s > 0)
            .unwrap_or(DEFAULT_SECTOR_SIZE),
        Err(_) => DEFAULT_SECTOR_SIZE,
    }
}

/// Whether this platform can bypass the page cache for source reads
/// (`--direct`). Linux has `O_DIRECT`.
pub const fn supports_direct() -> bool {
    true
}

/// Open the source with `O_DIRECT` so copy reads bypass the page cache.
pub fn open_source_direct(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECT)
        .open(path)
        .map_err(|e| {
            Error::io(
                format!("opening source '{}' with O_DIRECT", path.display()),
                e,
            )
        })
}

/// Hint the kernel to stop reading ahead on the source. Without this, a buffered
/// read near a bad sector pulls a wider read-ahead window into one failing bio,
/// ballooning a single bad byte into a much larger `bad` region; turning it off
/// caps the loss at one page. Advisory — the result is ignored.
pub fn disable_readahead(file: &File) {
    // SAFETY: posix_fadvise on a valid fd with constant arguments; its return is
    // advisory and intentionally ignored (read-ahead simply stays on if it fails).
    unsafe {
        nix::libc::posix_fadvise(file.as_raw_fd(), 0, 0, nix::libc::POSIX_FADV_RANDOM);
    }
}

/// Whether a read error means "this spot of the medium is unreadable" — the
/// only condition under which sectors are marked `bad` and the copy moves on.
/// `EIO` is the block layer's damage signal, `EREMOTEIO` its USB-bridge
/// cousin, `EBADMSG` an integrity (T10 DIF) failure. Anything else — `EINVAL`
/// from `O_DIRECT` misuse, a vanished device, a bad fd — is not positional
/// damage, and skipping past it would sweep the rest of the source into `bad`.
pub fn is_media_error(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(nix::libc::EIO | nix::libc::EREMOTEIO | nix::libc::EBADMSG)
    )
}

/// Block-device `(major, minor)` pairs backing a mounted filesystem or active
/// swap. Empty when `/proc` is unreadable.
pub fn mounted_dev_ids() -> HashSet<(u64, u64)> {
    let mut ids = HashSet::new();
    if let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") {
        for (id, source) in parse_mountinfo(&text) {
            ids.insert(id);
            // Btrfs (and other multi-device filesystems) report an anonymous
            // 0:xx id above; the mount source names the real block device.
            if let Some(path) = source.filter(|s| s.starts_with('/')) {
                if let Ok(meta) = std::fs::metadata(path) {
                    if meta.file_type().is_block_device() {
                        ids.insert(dev_split(meta.rdev()));
                    }
                }
            }
        }
    }
    // Swap does not appear in mountinfo but is just as live.
    if let Ok(text) = std::fs::read_to_string("/proc/swaps") {
        for line in text.lines().skip(1) {
            if let Some(path) = line.split_whitespace().next() {
                if let Ok(meta) = std::fs::metadata(path) {
                    ids.insert(dev_split(meta.rdev()));
                }
            }
        }
    }
    ids
}

/// Extract, per mountinfo line, the `major:minor` device field (index 2) and
/// the mount source — the second field after the `-` separator, e.g.
/// `/dev/mapper/root`. Virtual filesystems yield 0:xx ids and non-path sources
/// (`tmpfs`, `cgroup2`); the caller filters those out.
fn parse_mountinfo(text: &str) -> impl Iterator<Item = ((u64, u64), Option<&str>)> {
    text.lines().filter_map(|line| {
        let mut fields = line.split_whitespace();
        let id = fields.nth(2)?;
        let (maj, min) = id.split_once(':')?;
        let id = (maj.parse().ok()?, min.parse().ok()?);
        let mut tail = fields.skip_while(|f| *f != "-").skip(2);
        Some((id, tail.next()))
    })
}

/// Whether `path` is a block device whose device family — itself, its
/// partitions, and stacked holders (recursively) — intersects `mounted`.
pub fn device_is_mounted(path: &Path, mounted: &HashSet<(u64, u64)>) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.file_type().is_block_device() {
        return false;
    }
    device_family(dev_split(meta.rdev()))
        .iter()
        .any(|id| mounted.contains(id))
}

/// The transitive device family of a block device, walked through sysfs:
/// partitions appear as subdirectories of `/sys/dev/block/<maj>:<min>` with
/// their own `dev` file, stacked devices (LVM, dm-crypt, MD) as entries in
/// `holders/`. Contains at least the device itself.
fn device_family(root: (u64, u64)) -> HashSet<(u64, u64)> {
    let mut family = HashSet::from([root]);
    let mut queue = vec![root];
    while let Some((maj, min)) = queue.pop() {
        let base = PathBuf::from(format!("/sys/dev/block/{maj}:{min}"));
        for dir in [base.clone(), base.join("holders")] {
            for id in block_children(&dir) {
                if family.insert(id) {
                    queue.push(id);
                }
            }
        }
    }
    family
}

/// The `(major, minor)` of every subdirectory of `dir` that carries a `dev`
/// file — the sysfs shape of partitions and holder devices.
fn block_children(dir: &Path) -> Vec<(u64, u64)> {
    let mut ids = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(text) = std::fs::read_to_string(entry.path().join("dev")) {
                if let Some((maj, min)) = text.trim().split_once(':') {
                    if let (Ok(maj), Ok(min)) = (maj.parse(), min.parse()) {
                        ids.push((maj, min));
                    }
                }
            }
        }
    }
    ids
}

/// Split a raw `st_rdev` into `(major, minor)`.
fn dev_split(rdev: u64) -> (u64, u64) {
    (
        u64::from(nix::libc::major(rdev)),
        u64::from(nix::libc::minor(rdev)),
    )
}

#[cfg(test)]
mod tests {
    use super::parse_mountinfo;

    #[test]
    fn mountinfo_yields_device_ids_and_sources() {
        let text = "\
36 35 98:0 / /mnt1 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
37 35 8:17 / /mnt2 rw,relatime shared:2 - ext4 /dev/sdb1 rw
38 35 0:25 / /sys/fs/cgroup ro - cgroup2 cgroup2 rw
39 35 0:31 /root / rw shared:1 - btrfs /dev/mapper/root rw,ssd";
        let entries: Vec<_> = parse_mountinfo(text).collect();
        assert_eq!(
            entries,
            vec![
                ((98, 0), Some("/dev/root")),
                ((8, 17), Some("/dev/sdb1")),
                ((0, 25), Some("cgroup2")),
                // Btrfs: anonymous device id; the source names the real one.
                ((0, 31), Some("/dev/mapper/root")),
            ]
        );
    }
}
