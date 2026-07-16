//! Preflight safety checks, run before the first byte is written: the classic
//! `dd` footguns — copying a file onto itself, writing to a mounted device, a
//! target too small for the copy — are refused up front instead of being
//! discovered as corruption or a write error hours in.
//!
//! All checks fail towards permissiveness on probe errors (an unreadable
//! `/proc` or `/sys` simply disables the mounted guard): they exist to catch
//! mistakes, not to make exhume unusable in odd environments.

use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

use indicatif::HumanBytes;
use tracing::warn;

use crate::error::{Error, Result};
use crate::platform::{device_is_mounted, mounted_dev_ids};

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

#[cfg(test)]
mod tests {
    use super::ensure_not_same_file;

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
