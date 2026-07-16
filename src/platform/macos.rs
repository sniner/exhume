//! macOS backend: `DKIOC*` disk ioctls, `F_NOCACHE`/`F_RDAHEAD` fcntls, and
//! mount probing via `getfsstat`.
//!
//! macOS device nodes come in pairs: the buffered block node `/dev/diskN` and
//! the raw character node `/dev/rdiskN` — the latter is the one to image
//! (it bypasses the tiny block-layer cache and is much faster). Both answer
//! the `DKIOC*` ioctls, so everything here treats "block or character device"
//! as a potential disk and lets a failing ioctl say otherwise.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::params::DEFAULT_SECTOR_SIZE;

/// nix-generated wrappers for the disk ioctls we need (`sys/disk.h`).
mod ioctl {
    // DKIOCGETBLOCKSIZE — logical block size, `_IOR('d', 24, uint32_t)`.
    nix::ioctl_read!(dkiocgetblocksize, b'd', 24, u32);
    // DKIOCGETBLOCKCOUNT — capacity in blocks, `_IOR('d', 25, uint64_t)`.
    nix::ioctl_read!(dkiocgetblockcount, b'd', 25, u64);
}

/// Whether the open file is a device node that might be a disk — block
/// (`/dev/diskN`) or character (`/dev/rdiskN`). The `DKIOC*` ioctls sort out
/// the impostors (`/dev/null`, ttys) by failing.
fn is_disk_node(file: &File) -> bool {
    file.metadata().is_ok_and(|m| {
        let ft = m.file_type();
        ft.is_block_device() || ft.is_char_device()
    })
}

/// Capacity of a disk device: block count × block size, per ioctl. `None` if
/// either ioctl fails (not a disk) or the product is zero.
fn disk_capacity(file: &File) -> Option<u64> {
    let fd = file.as_raw_fd();
    let mut count: u64 = 0;
    let mut block: u32 = 0;
    // SAFETY: each ioctl writes one integer through the pointer; the fd is
    // valid for the duration of the call.
    unsafe {
        ioctl::dkiocgetblockcount(fd, &raw mut count).ok()?;
        ioctl::dkiocgetblocksize(fd, &raw mut block).ok()?;
    }
    count.checked_mul(u64::from(block)).filter(|&c| c > 0)
}

/// Detect the size of a source: regular files report their length via seeking
/// to the end. Disk devices report `0` there on macOS — their capacity comes
/// from the driver (`DKIOCGETBLOCKCOUNT × DKIOCGETBLOCKSIZE`) instead.
pub fn detect_size(file: &mut File) -> Option<u64> {
    let seeked = file.seek(SeekFrom::End(0)).ok();
    if matches!(seeked, Some(s) if s > 0) || !is_disk_node(file) {
        return seeked;
    }
    disk_capacity(file).or(seeked)
}

/// Detect the logical sector size of a source. Disk devices report it via
/// `DKIOCGETBLOCKSIZE`; everything else (regular files, pipes, an ioctl
/// failure) falls back to [`DEFAULT_SECTOR_SIZE`].
pub fn detect_sector_size(file: &File) -> u64 {
    if !is_disk_node(file) {
        return DEFAULT_SECTOR_SIZE;
    }
    let mut block: u32 = 0;
    // SAFETY: DKIOCGETBLOCKSIZE writes a single u32 through the pointer; the
    // fd is valid for the duration of the call.
    match unsafe { ioctl::dkiocgetblocksize(file.as_raw_fd(), &raw mut block) } {
        Ok(_) if block > 0 => u64::from(block),
        _ => DEFAULT_SECTOR_SIZE,
    }
}

/// Whether this platform can bypass the page cache for source reads
/// (`--direct`). macOS has `fcntl(F_NOCACHE)`.
pub const fn supports_direct() -> bool {
    true
}

/// Open the source for cache-bypassing reads. macOS has no `O_DIRECT`; the
/// equivalent is `F_NOCACHE` on an ordinarily opened fd. Unlike `O_DIRECT` it
/// imposes no alignment requirements, so the engine's aligned reads simply
/// keep working.
pub fn open_source_direct(path: &Path) -> Result<File> {
    let file = OpenOptions::new().read(true).open(path).map_err(|e| {
        Error::io(
            format!("opening source '{}' for F_NOCACHE reads", path.display()),
            e,
        )
    })?;
    // SAFETY: fcntl F_NOCACHE on a valid fd with constant arguments.
    let rc = unsafe { nix::libc::fcntl(file.as_raw_fd(), nix::libc::F_NOCACHE, 1) };
    if rc == -1 {
        return Err(Error::io(
            format!("enabling F_NOCACHE on source '{}'", path.display()),
            std::io::Error::last_os_error(),
        ));
    }
    Ok(file)
}

/// Hint the kernel to stop reading ahead on the source, so a read near a bad
/// sector does not balloon into a much larger `bad` region. Advisory — the
/// result is ignored.
pub fn disable_readahead(file: &File) {
    // SAFETY: fcntl F_RDAHEAD with constant arguments; its return is advisory
    // and intentionally ignored (read-ahead simply stays on if it fails).
    unsafe {
        nix::libc::fcntl(file.as_raw_fd(), nix::libc::F_RDAHEAD, 0);
    }
}

/// Whether a read error means "this spot of the medium is unreadable" — the
/// only condition under which sectors are marked `bad` and the copy moves on.
/// `EIO` is the damage signal, `EBADMSG` an integrity failure (Linux' third
/// kind, `EREMOTEIO`, does not exist here). Anything else — a vanished
/// device, a bad fd — is not positional damage, and skipping past it would
/// sweep the rest of the source into `bad`.
pub fn is_media_error(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(nix::libc::EIO | nix::libc::EBADMSG))
}

/// Block-device `(major, minor)` pairs backing a mounted filesystem, via
/// `getfsstat`. Empty on error. macOS swaps to files on an APFS volume that
/// itself shows up here, so there is no separate swap probe.
pub fn mounted_dev_ids() -> HashSet<(u64, u64)> {
    let mut ids = HashSet::new();
    for source in mount_sources() {
        if let Ok(meta) = std::fs::metadata(&source) {
            if meta.file_type().is_block_device() {
                ids.insert(dev_split(meta.rdev()));
            }
        }
    }
    ids
}

/// Every mounted filesystem's mount source (`f_mntfromname`) that names a
/// path — `/dev/disk3s1s1` and friends. Virtual sources (`devfs`,
/// `map auto_home`) are filtered out by the missing leading `/`.
fn mount_sources() -> Vec<PathBuf> {
    // First call sizes the buffer (null buffer = count only); MNT_NOWAIT
    // skips waiting on unresponsive (network) filesystems.
    // SAFETY: the null-buffer form is the documented way to ask for the count.
    let n = unsafe { nix::libc::getfsstat(std::ptr::null_mut(), 0, nix::libc::MNT_NOWAIT) };
    let Ok(n) = usize::try_from(n) else {
        return Vec::new();
    };
    // SAFETY: statfs is a plain C struct of integers and char arrays; zeroed
    // is a valid (if meaningless) value, overwritten by getfsstat below.
    let mut buf = vec![unsafe { std::mem::zeroed::<nix::libc::statfs>() }; n];
    let Ok(bytes) = i32::try_from(std::mem::size_of_val(&buf[..])) else {
        return Vec::new();
    };
    // SAFETY: the buffer holds `n` statfs entries and `bytes` says exactly
    // that; getfsstat fills at most that many.
    let filled = unsafe { nix::libc::getfsstat(buf.as_mut_ptr(), bytes, nix::libc::MNT_NOWAIT) };
    let Ok(filled) = usize::try_from(filled) else {
        return Vec::new();
    };
    buf.truncate(filled.min(n));
    buf.iter()
        .filter_map(|fs| {
            let name = c_buf_to_str(&fs.f_mntfromname)?;
            name.starts_with('/').then(|| PathBuf::from(name))
        })
        .collect()
}

/// Whether `path` is a disk device whose whole-disk unit carries a mounted
/// filesystem: the disk itself or any of its slices, raw or buffered —
/// `/dev/rdisk3` matches a mounted `/dev/disk3s1s1`. Related by *name*, since
/// macOS has no sysfs to walk.
///
/// Known limitation: APFS-synthesized containers are not traced to their
/// physical store — a mounted `disk3s1` living in a container on `disk0s2`
/// does not flag `disk0`.
pub fn device_is_mounted(path: &Path, mounted: &HashSet<(u64, u64)>) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let ft = meta.file_type();
    if !ft.is_block_device() && !ft.is_char_device() {
        return false;
    }
    let unit = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(disk_unit);
    let Some(unit) = unit else {
        // Not a diskN-style node; all we can compare is its own id.
        return mounted.contains(&dev_split(meta.rdev()));
    };
    device_family(unit).iter().any(|id| mounted.contains(id))
}

/// The whole-disk unit of a device-node name: `disk3s1s1` → `disk3`,
/// `rdisk3s1` → `disk3`. `None` for anything that is not a disk node.
fn disk_unit(name: &str) -> Option<&str> {
    let base = name.strip_prefix('r').unwrap_or(name);
    let rest = base.strip_prefix("disk")?;
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    Some(&base[..4 + digits])
}

/// The `(major, minor)` ids of every `/dev` node belonging to a whole-disk
/// unit: the disk itself and its slices, buffered and raw alike. Contains at
/// least the device itself (when `/dev` is readable, which it always is).
fn device_family(unit: &str) -> HashSet<(u64, u64)> {
    let mut family = HashSet::new();
    if let Ok(entries) = std::fs::read_dir("/dev") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if disk_unit(name) == Some(unit) {
                if let Ok(meta) = entry.metadata() {
                    family.insert(dev_split(meta.rdev()));
                }
            }
        }
    }
    family
}

/// Split a raw `st_rdev` into `(major, minor)`.
fn dev_split(rdev: u64) -> (u64, u64) {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        reason = "dev_t is a 32-bit value on macOS and major/minor numbers are non-negative"
    )]
    {
        let dev = rdev as nix::libc::dev_t;
        (nix::libc::major(dev) as u64, nix::libc::minor(dev) as u64)
    }
}

/// The `str` content of a NUL-terminated, fixed-size C string field.
fn c_buf_to_str(buf: &[nix::libc::c_char]) -> Option<&str> {
    // SAFETY: c_char and u8 have identical size and alignment; the cast only
    // reinterprets the bytes, and the borrow keeps the slice alive.
    let bytes = unsafe { &*(std::ptr::from_ref::<[nix::libc::c_char]>(buf) as *const [u8]) };
    std::ffi::CStr::from_bytes_until_nul(bytes)
        .ok()?
        .to_str()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::disk_unit;

    #[test]
    fn disk_unit_strips_slices_and_raw_prefix() {
        assert_eq!(disk_unit("disk3"), Some("disk3"));
        assert_eq!(disk_unit("disk3s1s1"), Some("disk3"));
        assert_eq!(disk_unit("rdisk14s2"), Some("disk14"));
        // Not disk nodes: no unit.
        assert_eq!(disk_unit("null"), None);
        assert_eq!(disk_unit("diskette"), None);
        assert_eq!(disk_unit("random"), None);
    }
}
