//! Preflight safety checks, run before the first byte is written: the classic
//! `dd` footguns — copying a file onto itself, writing to a mounted device, a
//! target too small for the copy — are refused up front instead of being
//! discovered as corruption or a write error hours in.
//!
//! All checks fail towards permissiveness on probe errors (an unreadable
//! `/proc` or `/sys` simply disables the mounted guard): they exist to catch
//! mistakes, not to make exhume unusable in odd environments.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use indicatif::HumanBytes;
use tracing::warn;

use crate::error::{Error, Result};

/// Refuse a source exhume cannot copy: FIFOs and sockets are not seekable, and
/// the engine reads by offset (`pread`), so they would die mid-run with a
/// cryptic "Illegal seek". Checked on the path *before* opening — opening a
/// FIFO with no writer would block forever. Character devices stay allowed
/// (they are generally pread-able, e.g. `/dev/zero`).
///
/// # Errors
///
/// Returns [`Error::Refused`] for a FIFO or socket source.
pub fn ensure_source_supported(source: &Path) -> Result<()> {
    let Ok(meta) = std::fs::metadata(source) else {
        return Ok(()); // let the open error report a missing source
    };
    let ft = meta.file_type();
    if ft.is_fifo() || ft.is_socket() {
        return Err(Error::Refused(format!(
            "source '{}' is a {} — exhume copies block devices and regular files, \
             not streams (use cat or dd for those)",
            source.display(),
            if ft.is_fifo() {
                "pipe (FIFO)"
            } else {
                "socket"
            }
        )));
    }
    Ok(())
}

/// Refuse to copy a file or device onto itself, including aliases via symlinks
/// or hardlinks: reads and writes would hit the same blocks.
///
/// # Errors
///
/// Returns [`Error::Refused`] when both paths resolve to the same file.
pub fn ensure_not_same_file(source: &Path, target: &Path) -> Result<()> {
    let (Ok(src), Ok(dst)) = (std::fs::metadata(source), std::fs::metadata(target)) else {
        return Ok(()); // a missing target is fine; open errors surface later
    };
    if src.dev() == dst.dev() && src.ino() == dst.ino() {
        return Err(Error::Refused(format!(
            "source '{}' and target '{}' are the same file; refusing to copy it onto itself",
            source.display(),
            target.display()
        )));
    }
    Ok(())
}

/// Refuse a block-device target that cannot hold `seek + domain` bytes,
/// instead of failing with a write error at the capacity boundary hours into
/// the copy. Regular files grow and are exempt; an unknown domain (`0`) cannot
/// be checked.
///
/// # Errors
///
/// Returns [`Error::Refused`] when the block-device target is too small.
pub fn ensure_target_capacity(dst: &File, target: &Path, seek: u64, domain: u64) -> Result<()> {
    if domain == 0 {
        return Ok(());
    }
    let Ok(meta) = dst.metadata() else {
        return Ok(());
    };
    if !meta.file_type().is_block_device() {
        return Ok(());
    }
    let mut handle = dst;
    let Ok(capacity) = handle.seek(SeekFrom::End(0)) else {
        return Ok(());
    };
    let needed = seek + domain;
    if needed > capacity {
        return Err(Error::Refused(format!(
            "target '{}' holds {} ({capacity} bytes) but the copy needs {} ({needed} bytes); \
             refusing before writing anything",
            target.display(),
            HumanBytes(capacity),
            HumanBytes(needed),
        )));
    }
    Ok(())
}

/// The mounted-device guard. A target block device that is mounted — itself,
/// one of its partitions, or a stacked device on top of it (LVM, dm-crypt),
/// including active swap — is refused unless `allow_mounted`. A mounted
/// *source* only warns: imaging a live filesystem is sometimes wanted, but the
/// result may be inconsistent. No-op where `/proc` is unavailable (non-Linux).
///
/// # Errors
///
/// Returns [`Error::Refused`] when the target is mounted and `allow_mounted`
/// is not set.
pub fn check_mounted(source: &Path, target: &Path, allow_mounted: bool) -> Result<()> {
    let mounted = mounted_dev_ids();
    if mounted.is_empty() {
        return Ok(());
    }
    if device_is_mounted(target, &mounted) {
        if allow_mounted {
            warn!(
                target = %target.display(),
                "target is mounted — proceeding on your say-so (--allow-mounted)"
            );
        } else {
            return Err(Error::Refused(format!(
                "target '{}' (or a partition / stacked device on it) is mounted; writing to it \
                 would corrupt the live filesystem — unmount it first, or pass --allow-mounted \
                 at your own risk",
                target.display()
            )));
        }
    }
    if device_is_mounted(source, &mounted) {
        warn!(
            source = %source.display(),
            "source is mounted — the image may be inconsistent; consider unmounting or \
             remounting read-only"
        );
    }
    Ok(())
}

/// Block-device `(major, minor)` pairs backing a mounted filesystem or active
/// swap. Empty when `/proc` is unreadable.
fn mounted_dev_ids() -> HashSet<(u64, u64)> {
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
fn device_is_mounted(path: &Path, mounted: &HashSet<(u64, u64)>) -> bool {
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
    use super::{ensure_not_same_file, parse_mountinfo};

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

    #[test]
    fn same_file_is_detected_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.img");
        let link = dir.path().join("alias.img");
        std::fs::write(&file, b"x").unwrap();
        std::os::unix::fs::symlink(&file, &link).unwrap();

        assert!(ensure_not_same_file(&file, &link).is_err());
        assert!(ensure_not_same_file(&file, &dir.path().join("other.img")).is_ok());
    }
}
