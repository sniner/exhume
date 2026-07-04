//! `--status`: render a state file for humans (or `--json`), without copying.
//!
//! The state file is readable TOML, but aggregating a region map and a hash
//! manifest in one's head is not fun. `--status` answers the actual questions:
//! how far is it, what is still missing, where are the bad spots, how much of
//! the manifest is filled.

use std::fmt::Write as _;
use std::path::PathBuf;

use indicatif::HumanBytes;
use serde::Serialize;

use crate::cli::Cli;
use crate::error::Result;
use crate::hash::chunk_count;
use crate::params::DEFAULT_TARGET;
use crate::region::RegionStatus;
use crate::state::StateFile;

/// How many bad regions the human rendering lists before summarising.
const BAD_REGION_LIMIT: usize = 20;

/// Everything `--status` reports about a state file.
#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub state: PathBuf,
    pub updated: chrono::DateTime<chrono::Utc>,
    pub source: PathBuf,
    pub target: PathBuf,
    pub sector_size: u64,
    pub transfer_size: u64,
    pub skip: u64,
    pub seek: u64,
    pub skip_unchanged: bool,
    pub skip_zeros: bool,
    /// Domain size in bytes (`0` = was unknown).
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub bytes_untried: u64,
    pub bytes_written: u64,
    pub bad_bytes: u64,
    /// All bad regions, in domain coordinates.
    pub bad_regions: Vec<BadRegion>,
    /// Manifest coverage, when the state carries hashes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hashes: Option<HashStatus>,
}

#[derive(Debug, Serialize)]
pub struct BadRegion {
    pub start: u64,
    pub length: u64,
}

#[derive(Debug, Serialize)]
pub struct HashStatus {
    pub algorithm: String,
    pub chunk_size: u64,
    pub chunks_total: u64,
    pub chunks_hashed: u64,
}

/// Load the state addressed by the command line and summarise it.
///
/// # Errors
///
/// Returns an error if the resolved state file does not exist or fails to
/// parse.
pub fn report(cli: &Cli) -> Result<StatusReport> {
    let path = resolve_state_path(cli);
    let state = StateFile::load(&path)?;
    let map = state.region_map();

    let bad_regions = map
        .regions_with(RegionStatus::Bad)
        .iter()
        .map(|r| BadRegion {
            start: r.start,
            length: r.length,
        })
        .collect();

    let bytes_total = state.progress.bytes_total;
    let hashes = state.hashes.as_ref().map(|h| HashStatus {
        algorithm: h.algorithm.clone(),
        chunk_size: h.chunk_size,
        chunks_total: if bytes_total > 0 {
            chunk_count(bytes_total, h.chunk_size)
        } else {
            h.chunks.len() as u64
        },
        chunks_hashed: h.chunks.iter().filter(|c| !c.is_empty()).count() as u64,
    });

    Ok(StatusReport {
        state: path,
        updated: state.meta.updated,
        source: state.params.source.clone(),
        target: state.params.target.clone(),
        sector_size: state.params.sector_size,
        transfer_size: state.params.transfer_size,
        skip: state.params.skip,
        seek: state.params.seek,
        skip_unchanged: state.params.skip_unchanged,
        skip_zeros: state.params.skip_zeros,
        bytes_total,
        bytes_done: map.bytes_with(RegionStatus::Done),
        bytes_untried: map.bytes_with(RegionStatus::Untried),
        bytes_written: state.progress.bytes_written,
        bad_bytes: map.bytes_with(RegionStatus::Bad),
        bad_regions,
        hashes,
    })
}

/// Where to look for the state: an explicit `STATE` wins; otherwise, if
/// `SOURCE` itself is a loadable state file, show that (the natural
/// `exhume --status run.state`); otherwise derive it from `TARGET` exactly
/// like a copy run would.
fn resolve_state_path(cli: &Cli) -> PathBuf {
    if let Some(state) = &cli.state {
        return state.clone();
    }
    if cli.target.is_none() && StateFile::load(&cli.source).is_ok() {
        return cli.source.clone();
    }
    let target = cli
        .target
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_TARGET));
    crate::engine::default_state_path(&target)
}

/// The human rendering of a report — one aligned block, bad regions capped.
#[must_use]
pub fn render(r: &StatusReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "State:    {} (updated {})",
        r.state.display(),
        r.updated.format("%Y-%m-%d %H:%M:%S UTC")
    );
    let _ = writeln!(
        out,
        "Copy:     {} -> {}",
        r.source.display(),
        r.target.display()
    );
    let _ = writeln!(
        out,
        "Grid:     {} sectors, {} transfer{}{}{}{}",
        HumanBytes(r.sector_size),
        HumanBytes(r.transfer_size),
        offset_phrase("skip", r.skip),
        offset_phrase("seek", r.seek),
        flag_phrase("skip-unchanged", r.skip_unchanged),
        flag_phrase("skip-zeros", r.skip_zeros),
    );
    let percent = if r.bytes_total > 0 {
        #[allow(
            clippy::cast_precision_loss,
            reason = "percentage display only; precision is irrelevant"
        )]
        let p = r.bytes_done as f64 / r.bytes_total as f64 * 100.0;
        format!(" ({p:.1}%)")
    } else {
        String::new()
    };
    let total = if r.bytes_total > 0 {
        format!("of {} ", HumanBytes(r.bytes_total))
    } else {
        String::new()
    };
    let _ = writeln!(
        out,
        "Progress: {} {}done{}, {} untried, {} bad",
        HumanBytes(r.bytes_done),
        total,
        percent,
        HumanBytes(r.bytes_untried),
        HumanBytes(r.bad_bytes),
    );
    let _ = writeln!(out, "Written:  {}", HumanBytes(r.bytes_written));
    if let Some(h) = &r.hashes {
        let _ = writeln!(
            out,
            "Hashes:   {} over {} chunks - {} of {} hashed",
            h.algorithm,
            HumanBytes(h.chunk_size),
            h.chunks_hashed,
            h.chunks_total,
        );
    }
    if !r.bad_regions.is_empty() {
        let _ = writeln!(out, "Bad regions ({}):", r.bad_regions.len());
        for region in r.bad_regions.iter().take(BAD_REGION_LIMIT) {
            let _ = writeln!(
                out,
                "  offset {:>14}  {}",
                region.start,
                HumanBytes(region.length)
            );
        }
        if r.bad_regions.len() > BAD_REGION_LIMIT {
            let _ = writeln!(
                out,
                "  ... and {} more",
                r.bad_regions.len() - BAD_REGION_LIMIT
            );
        }
    }
    out
}

/// `, skip 1.00 MiB` when nonzero, empty otherwise.
fn offset_phrase(label: &str, value: u64) -> String {
    if value > 0 {
        format!(", {label} {}", HumanBytes(value))
    } else {
        String::new()
    }
}

/// `, skip-unchanged` when set, empty otherwise.
fn flag_phrase(label: &str, value: bool) -> String {
    if value {
        format!(", {label}")
    } else {
        String::new()
    }
}
