//! Resolved run parameters.
//!
//! These describe *how* a copy runs and are persisted verbatim in the state
//! file's `[params]` section so a run can be repeated without re-specifying
//! anything. When resuming, the precedence is **CLI flag > state file >
//! built-in default**.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::cli::Cli;
use crate::error::{Error, Result};

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

/// Require that a byte offset `value` (the `--{label}` flag) is a whole number
/// of `sector`-sized blocks. Offsets must sit on the sector grid so the region
/// map stays aligned and `O_DIRECT` can be added later; `--length` is exempt (its
/// tail simply rounds up). On a violation the error suggests the nearest aligned
/// values as bare integers, with a human form appended only when it is exact.
///
/// # Errors
///
/// Returns [`Error::Misaligned`] if `value` is not a multiple of `sector`.
pub fn require_sector_aligned(label: &str, value: u64, sector: u64) -> Result<()> {
    if sector == 0 || value % sector == 0 {
        return Ok(());
    }
    let down = value - value % sector;
    let up = down + sector;
    Err(Error::Misaligned(format!(
        "--{label} {value} is not a multiple of the {sector}-byte sector size; try {} or {}",
        aligned_label(down, "down"),
        aligned_label(up, "up"),
    )))
}

/// Render an aligned suggestion like `102400 (up, 100K)` — the bare byte value,
/// the direction, and a human-readable form only when it divides exactly.
fn aligned_label(value: u64, dir: &str) -> String {
    match human_if_exact(value) {
        Some(h) => format!("{value} ({dir}, {h})"),
        None => format!("{value} ({dir})"),
    }
}

/// A short binary human-readable rendering (`1K`, `4M`, `2G`) of `n` — only when
/// it divides cleanly into a small multiple of a unit. The quotient bound keeps
/// the hint tidy: every sector-aligned value is a multiple of 1024, so without
/// it a value like 12345344 would render as the unhelpful `12056K`.
fn human_if_exact(n: u64) -> Option<String> {
    if n == 0 {
        return None;
    }
    for (unit, suffix) in [(1u64 << 30, 'G'), (1 << 20, 'M'), (1 << 10, 'K')] {
        if n % unit == 0 {
            let quotient = n / unit;
            return (quotient < 1000).then(|| format!("{quotient}{suffix}"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{human_if_exact, require_sector_aligned};

    #[test]
    fn aligned_offsets_pass() {
        assert!(require_sector_aligned("skip", 0, 512).is_ok());
        assert!(require_sector_aligned("skip", 4096, 512).is_ok());
        assert!(require_sector_aligned("seek", 8192, 4096).is_ok());
    }

    #[test]
    fn misaligned_offset_suggests_both_neighbours() {
        let err = require_sector_aligned("skip", 12_345_678, 4096).unwrap_err();
        let msg = err.to_string();
        // 12345678 = 3014 * 4096 + 334 → down 12345344, up 12349440.
        assert!(msg.contains("--skip 12345678"), "{msg}");
        assert!(msg.contains("12345344 (down)"), "{msg}");
        assert!(msg.contains("12349440 (up)"), "{msg}");
    }

    #[test]
    fn exact_suggestion_gets_a_human_form() {
        // 100001 with a 512 sector → down 99840, up 100352 (neither a clean
        // unit), but pick a case that lands exactly: 100K-aligned grid.
        let err = require_sector_aligned("seek", 102_400 + 1, 1024).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("102400 (down, 100K)"), "{msg}");
    }

    #[test]
    fn human_if_exact_picks_the_largest_unit() {
        assert_eq!(human_if_exact(1 << 30).as_deref(), Some("1G"));
        assert_eq!(human_if_exact(2 << 20).as_deref(), Some("2M"));
        assert_eq!(human_if_exact(100 << 10).as_deref(), Some("100K"));
        assert_eq!(human_if_exact(0), None);
        assert_eq!(human_if_exact(1000), None);
    }
}
