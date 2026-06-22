//! Resolved run parameters.
//!
//! These describe *how* a copy runs and are persisted verbatim in the state
//! file's `[params]` section so a run can be repeated without re-specifying
//! anything. When resuming, the precedence is **CLI flag > state file >
//! built-in default**.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::cli::Cli;

/// Default target when none is given on the command line.
pub const DEFAULT_TARGET: &str = "grave.img";

/// Hard fallback sector size, used only when the real one cannot be read (a
/// regular file, a pipe, or a failed `BLKSSZGET`). Block devices report their
/// actual logical sector size and never hit this. 512 is the universal floor:
/// it never over-aligns and never rejects a `skip` a device would accept.
pub const DEFAULT_SECTOR_SIZE: u64 = 512;

/// Desired I/O transfer size for healthy reads: 1 MiB. The effective size is
/// this aligned down to a multiple of the sector size. Pure throughput knob —
/// data loss on a read error is bounded by the sector size, not this.
pub const DESIRED_TRANSFER_SIZE: u64 = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunParams {
    pub source: PathBuf,
    pub target: PathBuf,
    /// Logical sector size: the atomic unit of readability, the alignment unit,
    /// and the granularity of error isolation.
    pub sector_size: u64,
    /// Desired transfer size for healthy reads (aligned down to a sector
    /// multiple at use). Also the granularity of the `--skip-*` modes.
    pub transfer_size: u64,
    #[serde(default)]
    pub skip: u64,
    #[serde(default)]
    pub seek: u64,
    /// Maximum bytes to copy; `0` means "the whole source".
    #[serde(default)]
    pub length: u64,
    /// Only write blocks that differ from the current target contents.
    #[serde(default)]
    pub skip_unchanged: bool,
    /// Don't write all-zero source blocks, keeping the target sparse.
    #[serde(default)]
    pub skip_zeros: bool,
}

impl RunParams {
    /// Merge command-line arguments over any parameters loaded from a prior
    /// state file, falling back to defaults. The source path always comes from
    /// the command line; `target` has already been resolved by the caller.
    /// `detected_sector` is the sector size probed from the source, used as the
    /// fallback for `sector_size` when neither the CLI nor the state set it.
    ///
    /// Precedence: CLI flag > state file > detected (sector) / built-in default.
    #[must_use]
    pub fn resolve(
        cli: &Cli,
        target: PathBuf,
        prior: Option<&RunParams>,
        detected_sector: u64,
    ) -> Self {
        RunParams {
            source: cli.source.clone(),
            target,
            sector_size: cli
                .sector_size
                .or_else(|| prior.map(|p| p.sector_size))
                .unwrap_or(detected_sector),
            transfer_size: cli
                .transfer_size
                .or_else(|| prior.map(|p| p.transfer_size))
                .unwrap_or(DESIRED_TRANSFER_SIZE),
            skip: cli.skip.or_else(|| prior.map(|p| p.skip)).unwrap_or(0),
            seek: cli.seek.or_else(|| prior.map(|p| p.seek)).unwrap_or(0),
            length: cli.length.or_else(|| prior.map(|p| p.length)).unwrap_or(0),
            // Sticky: once a run is recorded as skip-unchanged, resumes keep it
            // unless explicitly... well, the flag can only turn it on. To turn
            // it off again, edit or remove the state file.
            skip_unchanged: cli.skip_unchanged || prior.is_some_and(|p| p.skip_unchanged),
            skip_zeros: cli.skip_zeros || prior.is_some_and(|p| p.skip_zeros),
        }
    }
}
