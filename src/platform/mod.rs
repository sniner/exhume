//! The platform boundary: everything OS-specific — device ioctls, page-cache
//! bypass, mount probing, media-error classification — lives behind this
//! module, so the engine and the safety checks stay portable POSIX.
//!
//! Every backend exports the same set of functions and `cfg` picks one at
//! compile time; there is no runtime dispatch. The fallback backend keeps
//! platforms without a dedicated backend compiling, with safely degraded
//! behavior: default sector size, no `--direct`, no mounted-device guard.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as imp;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod fallback;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use fallback as imp;

pub use imp::{
    detect_sector_size, detect_size, device_is_mounted, disable_readahead, is_media_error,
    mounted_dev_ids, open_source_direct, supports_direct,
};
