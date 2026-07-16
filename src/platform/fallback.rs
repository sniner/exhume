//! Portable POSIX fallback for platforms without a dedicated backend (macOS,
//! the BSDs). Everything degrades safely instead of failing: the sector size
//! falls back to the default, `--direct` is unavailable, and the mounted
//! guard is disabled — the safety checks fail towards permissiveness.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::Path;

use crate::error::Result;
use crate::params::DEFAULT_SECTOR_SIZE;

/// Detect the size of a source: regular files report their length via seeking
/// to the end. Device capacities need OS-specific ioctls a dedicated backend
/// would provide; where the seek fails or reports nothing, the copy runs
/// open-ended (`None` / `0`).
pub fn detect_size(file: &mut File) -> Option<u64> {
    file.seek(SeekFrom::End(0)).ok()
}

/// Probing the logical sector size needs OS-specific ioctls; assume the
/// default.
pub fn detect_sector_size(_file: &File) -> u64 {
    DEFAULT_SECTOR_SIZE
}

/// Whether this platform can bypass the page cache for source reads
/// (`--direct`). Not without a dedicated backend.
pub const fn supports_direct() -> bool {
    false
}

/// Never called: the engine checks [`supports_direct`] first and ignores
/// `--direct` with a warning.
pub fn open_source_direct(_path: &Path) -> Result<File> {
    unreachable!("open_source_direct is only reached where supports_direct() is true")
}

/// Read-ahead stays on: turning it off needs OS-specific advice calls.
/// Purely an error-containment optimization, never correctness.
pub fn disable_readahead(_file: &File) {}

/// Whether a read error means "this spot of the medium is unreadable" — the
/// only condition under which sectors are marked `bad` and the copy moves on.
/// `EIO` is the damage signal, `EBADMSG` an integrity failure (Linux' third
/// kind, `EREMOTEIO`, exists nowhere else). Anything else is not positional
/// damage, and skipping past it would sweep the rest of the source into `bad`.
pub fn is_media_error(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(nix::libc::EIO | nix::libc::EBADMSG))
}

/// Mount probing needs an OS-specific source (procfs, `getfsstat`); reporting
/// nothing disables the mounted guard entirely.
pub fn mounted_dev_ids() -> HashSet<(u64, u64)> {
    HashSet::new()
}

/// Unreachable in practice: [`mounted_dev_ids`] reports nothing, and the
/// mounted guard bails out on an empty set before probing any device.
pub fn device_is_mounted(_path: &Path, _mounted: &HashSet<(u64, u64)>) -> bool {
    false
}
