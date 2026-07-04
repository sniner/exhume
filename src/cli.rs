//! Command-line interface.
//!
//! `exhume [SOURCE] [TARGET] [STATE]` — three positional arguments, the last
//! two optional. Flags that take a size accept human-readable suffixes
//! (`512`, `64K`, `1M`, `1.5G`, `4KiB`, `1MB`).

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "a CLI flag struct naturally carries several independent bool toggles"
)]
#[command(
    name = "exhume",
    author,
    version,
    about = "Modern, resumable disk imaging and rescue tool",
    long_about = "Reads SOURCE block-wise and writes it to TARGET, recording progress in a \
                  human-readable TOML state file. Interrupted runs resume by simply re-running \
                  the same command; read errors are recorded and skipped instead of aborting."
)]
pub struct Cli {
    /// Source: block device or file to read from
    pub source: PathBuf,

    /// Target: block device or file to write to [default: grave.img]
    pub target: Option<PathBuf>,

    /// State file (TOML) [default: <target>.state]
    pub state: Option<PathBuf>,

    /// Logical sector size — the alignment and recovery granularity.
    /// Auto-detected from block devices; override only if you must (e.g. 512, 4K)
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub sector_size: Option<u64>,

    /// I/O transfer size for healthy reads (default 1M). Aligned down to a
    /// multiple of the sector size. Smaller values reduce skip-* granularity
    #[arg(short, long, value_name = "SIZE", value_parser = parse_size)]
    pub transfer_size: Option<u64>,

    /// Copy at most this many bytes (0 = whole source)
    #[arg(short, long, value_name = "SIZE", value_parser = parse_size)]
    pub length: Option<u64>,

    /// Skip this many bytes at the start of the source
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub skip: Option<u64>,

    /// Seek this many bytes into the target before writing
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub seek: Option<u64>,

    /// Only write blocks that differ from the target (reads the target to
    /// compare). For refreshing an existing image/clone — not first imaging.
    /// Sticky across resumes; =false switches it off again
    #[arg(long, num_args = 0..=1, default_missing_value = "true",
          require_equals = true, action = clap::ArgAction::Set)]
    pub skip_unchanged: Option<bool>,

    /// Don't write all-zero source blocks, keeping the target sparse. Assumes a
    /// fresh/zeroed target; use --skip-unchanged to refresh an existing target.
    /// Sticky across resumes; =false switches it off again
    #[arg(long, num_args = 0..=1, default_missing_value = "true",
          require_equals = true, action = clap::ArgAction::Set)]
    pub skip_zeros: Option<bool>,

    /// Record per-chunk BLAKE3 digests in the state file (an integrity
    /// manifest for --verify). On by default when STATE is named explicitly;
    /// =false switches it off, --hash forces it on for an auto-named state
    #[arg(long, num_args = 0..=1, default_missing_value = "true",
          require_equals = true, action = clap::ArgAction::Set)]
    pub hash: Option<bool>,

    /// Chunk size of the hash manifest (default 64M). Must be a multiple of
    /// the sector size; fixed once recorded in the state file
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub hash_chunk: Option<u64>,

    /// Re-scan a completed state against its source: chunks whose hash still
    /// matches the manifest are skipped entirely (no target I/O); changed
    /// chunks are written block-wise with target comparison. Add
    /// --skip-unchanged to compare against the target instead (slower, also
    /// repairs target-side rot)
    #[arg(long)]
    pub refresh: bool,

    /// After the copy, read the target back and check it against the hash
    /// manifest (exit code 3 on mismatch). Re-running the same command on a
    /// completed state verifies without copying — e.g. months later
    #[arg(long)]
    pub verify: bool,

    /// Re-read regions marked bad in a previous run and recover what is now
    /// readable (one pass; re-run for more). Reads at sector granularity
    #[arg(long)]
    pub retry: bool,

    /// After the run, also write the region map as a GNU ddrescue mapfile, to
    /// hand an unfinished rescue over to ddrescue where exhume left off
    #[arg(long, value_name = "PATH")]
    pub export_map: Option<PathBuf>,

    /// Read the source with O_DIRECT, bypassing the page cache so a re-read
    /// actually hits the medium (Linux only; for failing devices)
    #[allow(
        clippy::doc_markdown,
        reason = "this doc comment is clap --help text; backticks would render literally"
    )]
    #[arg(long)]
    pub direct: bool,

    /// Show the state (progress, bad regions, manifest coverage) without
    /// copying anything. Give the usual arguments, or the state file directly:
    /// exhume --status run.state
    #[arg(long)]
    pub status: bool,

    /// Overwrite an existing, non-empty target
    #[arg(short, long)]
    pub force: bool,

    /// Proceed even if the target device (or a partition / stacked device on
    /// it) is mounted. Dangerous: writing under a live filesystem corrupts it
    #[arg(long)]
    pub allow_mounted: bool,

    /// Suppress the progress bar
    #[arg(short, long)]
    pub quiet: bool,

    /// Print the final summary as a single JSON object on stdout instead of
    /// human-readable text (the progress bar still draws to stderr)
    #[arg(long)]
    pub json: bool,

    /// Emit NDJSON progress events on stdout (one line every ~2s per phase)
    /// and the final summary as a compact JSON line — for cron jobs, GUIs,
    /// and orchestration. Implies --json for the summary
    #[arg(long)]
    pub json_progress: bool,

    /// Increase log verbosity (-v info, -vv debug, -vvv trace)
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

/// Parse a human-readable byte size. Bare numbers are bytes; `K/M/G/T` are
/// binary multiples (1024-based), the explicit `KiB/MiB/...` are too, and the
/// SI `KB/MB/...` are 1000-based. Fractional values (`1.5M`) are allowed.
///
/// # Errors
///
/// Returns a human-readable message if the number is malformed or negative, or
/// the unit suffix is unknown.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "value is validated non-negative; sizes well within f64's exact integer range"
)]
pub fn parse_size(input: &str) -> std::result::Result<u64, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty size".to_string());
    }
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '_')
        .unwrap_or(s.len());
    let (number, unit) = s.split_at(split);
    let value: f64 = number
        .replace('_', "")
        .parse()
        .map_err(|_| format!("invalid number in size '{input}'"))?;
    if value < 0.0 {
        return Err(format!("size must not be negative: '{input}'"));
    }
    let multiplier: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        "t" | "tib" => 1 << 40,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        other => return Err(format!("unknown size unit '{other}' in '{input}'")),
    };
    Ok((value * multiplier as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn parses_bare_bytes() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("1024b").unwrap(), 1024);
    }

    #[test]
    fn parses_binary_suffixes() {
        assert_eq!(parse_size("1K").unwrap(), 1024);
        assert_eq!(parse_size("64K").unwrap(), 64 * 1024);
        assert_eq!(parse_size("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("1MiB").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn parses_si_suffixes_and_fractions() {
        assert_eq!(parse_size("1kb").unwrap(), 1000);
        assert_eq!(parse_size("1MB").unwrap(), 1_000_000);
        assert_eq!(parse_size("1.5M").unwrap(), 1024 * 1024 + 512 * 1024);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("12x").is_err());
        assert!(parse_size("-5").is_err());
    }
}
