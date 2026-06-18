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

/// Default block size: 1 MiB. Large enough for fast imaging of healthy media,
/// small enough that a single bad block does not lose much on failing media.
pub const DEFAULT_BLOCK_SIZE: u64 = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunParams {
    pub source: PathBuf,
    pub target: PathBuf,
    pub block_size: u64,
    #[serde(default)]
    pub skip: u64,
    #[serde(default)]
    pub seek: u64,
    /// Maximum bytes to copy; `0` means "the whole source".
    #[serde(default)]
    pub count: u64,
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
    #[must_use]
    pub fn resolve(cli: &Cli, target: PathBuf, prior: Option<&RunParams>) -> Self {
        RunParams {
            source: cli.source.clone(),
            target,
            block_size: cli
                .block_size
                .or_else(|| prior.map(|p| p.block_size))
                .unwrap_or(DEFAULT_BLOCK_SIZE),
            skip: cli.skip.or_else(|| prior.map(|p| p.skip)).unwrap_or(0),
            seek: cli.seek.or_else(|| prior.map(|p| p.seek)).unwrap_or(0),
            count: cli.count.or_else(|| prior.map(|p| p.count)).unwrap_or(0),
            // Sticky: once a run is recorded as skip-unchanged, resumes keep it
            // unless explicitly... well, the flag can only turn it on. To turn
            // it off again, edit or remove the state file.
            skip_unchanged: cli.skip_unchanged || prior.is_some_and(|p| p.skip_unchanged),
            skip_zeros: cli.skip_zeros || prior.is_some_and(|p| p.skip_zeros),
        }
    }
}
