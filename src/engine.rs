//! The copy engine: parameter resolution, overwrite safety, size detection,
//! the block-wise copy loop, and graceful interruption.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Seek, SeekFrom};
use std::os::unix::fs::{FileExt, FileTypeExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tracing::{info, warn};

use crate::cli::Cli;
use crate::error::{Error, Result};
use crate::params::{DEFAULT_SECTOR_SIZE, DEFAULT_TARGET, RunParams, require_sector_aligned};
use crate::progress::Reporter;
use crate::region::{RegionMap, RegionStatus};
use crate::state::StateFile;

/// How often to flush the state file to disk during a long copy.
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);

/// Set by the SIGINT/SIGTERM handler so the copy loop can stop and flush.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Outcome of a run, handed to the binary for rendering and exit-code mapping.
#[derive(Debug, Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "plain result DTO; these booleans are independent outcome facts"
)]
pub struct Summary {
    pub source: PathBuf,
    pub target: PathBuf,
    pub state_path: PathBuf,
    /// Domain size in bytes (`0` = was unknown).
    pub bytes_total: u64,
    /// Bytes scanned and accounted for (read from source, marked done).
    pub bytes_done: u64,
    /// Bytes actually written to the target (< `bytes_done` in skip-unchanged mode).
    pub bytes_written: u64,
    pub bad_bytes: u64,
    pub bad_regions: usize,
    /// Whether `--skip-unchanged` was active (affects how the summary reads).
    pub skip_unchanged: bool,
    /// Whether `--skip-zeros` was active (affects how the summary reads).
    pub skip_zeros: bool,
    /// Whole domain copied with no errors and no interruption.
    pub completed: bool,
    pub interrupted: bool,
}

/// Run a copy as described by the command line, resuming from a state file if
/// one exists.
///
/// # Errors
///
/// Returns an error if arguments are invalid, the target needs `--force`, the
/// source or target cannot be opened, a write fails, or the state file cannot
/// be read or written.
pub fn run(cli: &Cli) -> Result<Summary> {
    let target = cli
        .target
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_TARGET));
    let state_path = cli
        .state
        .clone()
        .unwrap_or_else(|| default_state_path(&target));

    let existing = StateFile::load_if_exists(&state_path)?;
    let resuming = existing.is_some();

    // Open the source early: the sector size is probed from it and feeds
    // parameter resolution.
    let mut src = File::open(&cli.source)
        .map_err(|e| Error::io(format!("opening source '{}'", cli.source.display()), e))?;
    let detected_sector = detect_sector_size(&src);

    let params = resolve_params(cli, target.clone(), existing.as_ref(), detected_sector)?;

    // Effective transfer size: the desired size aligned down to whole sectors,
    // never below one sector.
    let transfer = align_down(params.transfer_size, params.sector_size).max(params.sector_size);

    // Overwrite safety: an existing, occupied target needs --force, unless a
    // state file is present (which signals an intentional resume).
    let occupied = target_occupied(&target)?;
    if !resuming && !cli.force && occupied {
        return Err(Error::TargetExists(target));
    }

    if let Some(prev) = &existing {
        if prev.params.source != params.source {
            warn!(
                old = %prev.params.source.display(),
                new = %params.source.display(),
                "source path differs from the one recorded in the state file"
            );
        }
        info!(state = %state_path.display(), "resuming from existing state file");
    }

    // --skip-zeros relies on the target reading as zero where writes are elided.
    // On an occupied target that is not guaranteed, so old data may survive.
    if params.skip_zeros && occupied && !resuming {
        warn!(
            target = %target.display(),
            "--skip-zeros leaves zero source blocks unwritten; pre-existing non-zero \
             data in the target will remain (sparse semantics assume a zeroed target) — \
             use --skip-unchanged to refresh an existing target correctly"
        );
    }

    install_signal_handler()?;

    // Never truncate: the target may be a block device or a partially written image.
    let dst = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&target)
        .map_err(|e| Error::io(format!("opening target '{}'", target.display()), e))?;

    let domain = domain_length(detect_size(&mut src), params.skip, params.length);

    let map = match &existing {
        Some(state) => state.region_map(),
        None => RegionMap::from_total(domain),
    };

    let created = existing.as_ref().map_or_else(Utc::now, |s| s.meta.created);

    let processed_start = domain.saturating_sub(map.bytes_with(RegionStatus::Untried));
    let reporter = Reporter::new(domain, processed_start, cli.quiet);
    let prior_written = existing.as_ref().map_or(0, |s| s.progress.bytes_written);

    let mut copier = Copier::new(
        &src,
        &dst,
        &params,
        &reporter,
        &state_path,
        created,
        domain,
        map,
        prior_written,
        transfer,
    );
    let interrupted = copier.drive(cli.retry)?;

    // Persist the final state regardless of how the loop ended.
    copier.flush()?;
    let (map, bytes_written) = copier.into_parts();
    reporter.finish();

    // Materialise a sparse tail: ensure a regular-file target spans the whole
    // processed domain even if trailing blocks were skipped (zero or unchanged).
    ensure_len(&dst, params.seek + map.covered_end())?;

    Ok(summarize(
        params,
        target,
        state_path,
        domain,
        &map,
        bytes_written,
        interrupted,
    ))
}

/// Resolve run parameters from CLI + prior state, then validate them against a
/// resumed state. The region map is aligned to the sector size recorded in the
/// state file, so a resume must not silently re-grid it against a different one.
fn resolve_params(
    cli: &Cli,
    target: PathBuf,
    existing: Option<&StateFile>,
    detected_sector: u64,
) -> Result<RunParams> {
    let params = RunParams::resolve(cli, target, existing.map(|s| &s.params), detected_sector);

    if params.sector_size == 0 || params.transfer_size == 0 {
        return Err(Error::InvalidSize(
            "sector size and transfer size must be greater than 0".to_string(),
        ));
    }

    if let Some(prev) = existing {
        if let Some(cli_sector) = cli.sector_size {
            if cli_sector != prev.params.sector_size {
                return Err(Error::InvalidSize(format!(
                    "--sector-size {} conflicts with the resumed state's sector size {} \
                     (the region map is aligned to the latter; edit or remove the state file \
                     to change it)",
                    cli_sector, prev.params.sector_size
                )));
            }
        } else if detected_sector != prev.params.sector_size {
            warn!(
                detected = detected_sector,
                recorded = prev.params.sector_size,
                "detected sector size differs from the state file — keeping the recorded value"
            );
        }
    }

    // Offsets must sit on the sector grid (the map stays aligned; O_DIRECT later
    // depends on it). `length` is exempt — its tail rounds up.
    require_sector_aligned("skip", params.skip, params.sector_size)?;
    require_sector_aligned("seek", params.seek, params.sector_size)?;

    Ok(params)
}

/// Assemble the run [`Summary`] from the final copy state.
fn summarize(
    params: RunParams,
    target: PathBuf,
    state_path: PathBuf,
    domain: u64,
    map: &RegionMap,
    bytes_written: u64,
    interrupted: bool,
) -> Summary {
    let bad_regions = map
        .regions()
        .iter()
        .filter(|r| r.status == RegionStatus::Bad)
        .count();
    let untried = map.bytes_with(RegionStatus::Untried);

    Summary {
        source: params.source,
        target,
        state_path,
        bytes_total: domain,
        bytes_done: map.bytes_with(RegionStatus::Done),
        bytes_written,
        bad_bytes: map.bytes_with(RegionStatus::Bad),
        bad_regions,
        skip_unchanged: params.skip_unchanged,
        skip_zeros: params.skip_zeros,
        completed: !interrupted && untried == 0 && bad_regions == 0,
        interrupted,
    }
}

/// Holds the live state of an in-flight copy so both copy strategies can share
/// flushing and the skip-unchanged comparison without long argument lists.
struct Copier<'a> {
    src: &'a File,
    dst: &'a File,
    params: &'a RunParams,
    reporter: &'a Reporter,
    state_path: &'a Path,
    created: DateTime<Utc>,
    domain: u64,
    /// Effective transfer size for healthy reads: the read/write chunk on the
    /// happy path, and the buffer size. Read errors drop to sector granularity.
    transfer: u64,
    map: RegionMap,
    /// Bytes actually written to the target (carried across resumes); excludes
    /// blocks skipped in skip-unchanged mode because they already matched.
    bytes_written: u64,
    buf: Vec<u8>,
    /// Scratch buffer for the skip-unchanged comparison; empty when disabled.
    cmp_buf: Vec<u8>,
    last_flush: Instant,
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "buffer indices are bounded by the transfer size, which fits in usize on supported targets"
)]
impl<'a> Copier<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        src: &'a File,
        dst: &'a File,
        params: &'a RunParams,
        reporter: &'a Reporter,
        state_path: &'a Path,
        created: DateTime<Utc>,
        domain: u64,
        map: RegionMap,
        bytes_written: u64,
        transfer: u64,
    ) -> Self {
        let buf_size = transfer as usize;
        let cmp_buf = if params.skip_unchanged {
            vec![0u8; buf_size]
        } else {
            Vec::new()
        };
        Copier {
            src,
            dst,
            params,
            reporter,
            state_path,
            created,
            domain,
            transfer,
            map,
            bytes_written,
            buf: vec![0u8; buf_size],
            cmp_buf,
            last_flush: Instant::now(),
        }
    }

    /// Drive the whole copy: for a known-size domain, process the untried
    /// regions and then, if `retry` is set, re-read the `bad` regions; for an
    /// unknown-size source, copy sequentially. Both passes use the same logic
    /// (read the transfer size, drop to sectors on error). Returns `true` if
    /// interrupted.
    fn drive(&mut self, retry: bool) -> Result<bool> {
        if self.domain == 0 {
            return self.run_sequential();
        }
        let mut stopped = self.process(RegionStatus::Untried, true)?;
        if !stopped && retry {
            let bad_before = self.map.bytes_with(RegionStatus::Bad);
            if bad_before > 0 {
                info!(bad_bytes = bad_before, "retrying bad regions");
                stopped = self.process(RegionStatus::Bad, false)?;
                let bad_after = self.map.bytes_with(RegionStatus::Bad);
                info!(
                    recovered = bad_before - bad_after,
                    remaining = bad_after,
                    "retry pass complete"
                );
            } else {
                info!("no bad regions to retry");
            }
        }
        Ok(stopped)
    }

    /// Read and copy every region currently in `status`, using transfer-sized
    /// reads. A read error drops to [`Self::isolate`], which re-reads the failed
    /// transfer block sector-by-sector so only the genuinely unreadable sectors
    /// are marked bad; writes are fatal. `advance` controls the progress bar:
    /// the first untried pass advances it, a retry pass re-scans already-counted
    /// bytes and passes `false`. Returns `true` if interrupted.
    fn process(&mut self, status: RegionStatus, advance: bool) -> Result<bool> {
        for region in self.map.regions_with(status) {
            let mut pos = region.start;
            let end = region.end();
            while pos < end {
                if interrupted() {
                    self.flush()?;
                    return Ok(true);
                }
                let want = self.transfer.min(end - pos);
                let src_off = self.params.skip + pos;
                match self.src.read_at(&mut self.buf[..want as usize], src_off) {
                    Ok(0) => {
                        warn!(
                            offset = src_off,
                            "unexpected end of source before end of region"
                        );
                        break;
                    }
                    Ok(n) => {
                        let dst_off = self.params.seek + pos;
                        self.write_block(dst_off, n)?;
                        self.map.mark(pos, n as u64, RegionStatus::Done);
                        if advance {
                            self.reporter.inc(n as u64);
                        }
                        pos += n as u64;
                    }
                    Err(e) => {
                        warn!(offset = src_off, len = want, error = %e, "read error — isolating bad sectors");
                        if self.isolate(pos, want, advance)? {
                            return Ok(true);
                        }
                        pos += want;
                    }
                }
                if self.last_flush.elapsed() >= FLUSH_INTERVAL {
                    self.flush()?;
                }
            }
        }
        Ok(false)
    }

    /// Re-read a failed transfer block `[start, start + len)` one sector at a
    /// time, so a single dead sector costs one sector instead of the whole
    /// transfer block. Readable sectors are written and marked done; unreadable
    /// ones are marked bad. A trailing partial sector (only at the tail of an
    /// odd-length regular file) is read as its remainder. `advance` mirrors
    /// [`Self::process`]. Returns `true` if interrupted.
    fn isolate(&mut self, start: u64, len: u64, advance: bool) -> Result<bool> {
        let sector = self.params.sector_size;
        let end = start + len;
        let mut pos = start;
        let mut bad = 0u64;
        while pos < end {
            if interrupted() {
                self.flush()?;
                return Ok(true);
            }
            let want = sector.min(end - pos);
            let src_off = self.params.skip + pos;
            match self.src.read_at(&mut self.buf[..want as usize], src_off) {
                // Unexpected EOF mid-block: stop, leaving the rest in its prior
                // status (so the run reports as incomplete rather than guessing).
                Ok(0) => break,
                Ok(n) => {
                    let dst_off = self.params.seek + pos;
                    self.write_block(dst_off, n)?;
                    self.map.mark(pos, n as u64, RegionStatus::Done);
                    if advance {
                        self.reporter.inc(n as u64);
                    }
                    pos += n as u64;
                }
                Err(_) => {
                    self.map.mark(pos, want, RegionStatus::Bad);
                    if advance {
                        self.reporter.inc(want);
                    }
                    bad += want;
                    pos += want;
                }
            }
            if self.last_flush.elapsed() >= FLUSH_INTERVAL {
                self.flush()?;
            }
        }
        if bad == 0 {
            info!(
                offset = self.params.skip + start,
                len, "transfer block fully recovered at sector granularity"
            );
        } else {
            warn!(
                offset = self.params.skip + start,
                bad_bytes = bad,
                "isolated unreadable sectors"
            );
        }
        Ok(false)
    }

    /// Copy a source of unknown size sequentially from the current frontier
    /// until end-of-input (or `count` bytes). Read errors are fatal here, since
    /// without a known domain we cannot safely skip ahead. Returns `true` if
    /// interrupted.
    fn run_sequential(&mut self) -> Result<bool> {
        let transfer = self.transfer;
        let mut pos = self.map.covered_end();
        loop {
            if interrupted() {
                self.flush()?;
                return Ok(true);
            }
            if self.params.length > 0 && pos >= self.params.length {
                break;
            }
            let want = if self.params.length > 0 {
                transfer.min(self.params.length - pos)
            } else {
                transfer
            };
            let src_off = self.params.skip + pos;
            let n = self
                .src
                .read_at(&mut self.buf[..want as usize], src_off)
                .map_err(|e| Error::io(format!("reading at source offset {src_off}"), e))?;
            if n == 0 {
                break;
            }
            let dst_off = self.params.seek + pos;
            self.write_block(dst_off, n)?;
            self.map.mark(pos, n as u64, RegionStatus::Done);
            self.reporter.inc(n as u64);
            pos += n as u64;

            if self.last_flush.elapsed() >= FLUSH_INTERVAL {
                self.flush()?;
            }
        }
        Ok(false)
    }

    /// Write `self.buf[..n]` to `dst_off`, unless a skip mode elides it:
    /// `--skip-zeros` drops all-zero source blocks (no target read), and
    /// `--skip-unchanged` drops blocks the target already holds. When both are
    /// active, a zero block is skipped first without consulting the target.
    fn write_block(&mut self, dst_off: u64, n: usize) -> Result<()> {
        let write = if self.params.skip_zeros && is_all_zero(&self.buf[..n]) {
            false
        } else if self.params.skip_unchanged {
            target_differs(self.dst, dst_off, &self.buf[..n], &mut self.cmp_buf)
        } else {
            true
        };
        if write {
            self.dst
                .write_all_at(&self.buf[..n], dst_off)
                .map_err(|e| {
                    Error::io(format!("writing {n} bytes at target offset {dst_off}"), e)
                })?;
            self.bytes_written += n as u64;
        }
        Ok(())
    }

    /// Serialise and atomically write the current state, and reset the flush timer.
    fn flush(&mut self) -> Result<()> {
        StateFile::build(
            self.params,
            &self.map,
            self.domain,
            self.created,
            self.bytes_written,
        )
        .save_atomic(self.state_path)?;
        self.last_flush = Instant::now();
        Ok(())
    }

    /// Consume the copier, returning the region map and the write counter.
    fn into_parts(self) -> (RegionMap, u64) {
        (self.map, self.bytes_written)
    }
}

/// In skip-unchanged mode: does the target already differ from `data` at
/// `dst_off`? A mismatch, short read, or read error all count as "differs", so
/// the safe action (write) is taken.
fn target_differs(dst: &File, dst_off: u64, data: &[u8], cmp_buf: &mut [u8]) -> bool {
    match dst.read_at(&mut cmp_buf[..data.len()], dst_off) {
        Ok(n) if n == data.len() => cmp_buf[..n] != *data,
        _ => true,
    }
}

/// Whether every byte in `buf` is zero.
fn is_all_zero(buf: &[u8]) -> bool {
    buf.iter().all(|&b| b == 0)
}

/// Ensure a regular-file target spans at least `len` bytes, extending it as a
/// sparse hole if shorter. Block devices and already-long-enough files are left
/// untouched; the target is never truncated.
fn ensure_len(file: &File, len: u64) -> Result<()> {
    let meta = file
        .metadata()
        .map_err(|e| Error::io("inspecting target before finalising its length", e))?;
    if meta.file_type().is_file() && meta.len() < len {
        file.set_len(len)
            .map_err(|e| Error::io(format!("extending target to {len} bytes"), e))?;
    }
    Ok(())
}

/// Append `.state` to the target path to form the default state-file path.
fn default_state_path(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".state");
    PathBuf::from(s)
}

/// Detect the size of a source: regular files report their length, block
/// devices report their capacity — both via seeking to the end. Non-seekable
/// sources (pipes, character devices) return `None`.
fn detect_size(file: &mut File) -> Option<u64> {
    file.seek(SeekFrom::End(0)).ok()
}

/// Round `value` down to a multiple of `align`. `align == 0` leaves it unchanged.
fn align_down(value: u64, align: u64) -> u64 {
    if align == 0 {
        value
    } else {
        value - value % align
    }
}

/// nix-generated wrappers for the block-device ioctls we need.
mod ioctl {
    // BLKSSZGET — logical sector size, `_IO(0x12, 104)` == 0x1268, returns an int.
    nix::ioctl_read_bad!(blksszget, 0x1268, nix::libc::c_int);
}

/// Detect the logical sector size of a source. Block devices report it via
/// `BLKSSZGET`; everything else (regular files, pipes, an ioctl failure) falls
/// back to [`DEFAULT_SECTOR_SIZE`].
fn detect_sector_size(file: &File) -> u64 {
    use std::os::unix::io::AsRawFd;
    let is_block = file
        .metadata()
        .is_ok_and(|m| m.file_type().is_block_device());
    if !is_block {
        return DEFAULT_SECTOR_SIZE;
    }
    let mut size: nix::libc::c_int = 0;
    // SAFETY: BLKSSZGET writes a single c_int through the pointer; the fd is
    // valid for the duration of the call.
    match unsafe { ioctl::blksszget(file.as_raw_fd(), &raw mut size) } {
        Ok(_) => u64::try_from(size)
            .ok()
            .filter(|&s| s > 0)
            .unwrap_or(DEFAULT_SECTOR_SIZE),
        Err(_) => DEFAULT_SECTOR_SIZE,
    }
}

/// Compute the copy domain length from the detected source size, `skip`, and
/// `length`. `0` means "unknown / copy until end-of-input".
fn domain_length(src_size: Option<u64>, skip: u64, length: u64) -> u64 {
    match (src_size.map(|s| s.saturating_sub(skip)), length) {
        (Some(available), 0) => available,
        (Some(available), limit) => available.min(limit),
        (None, 0) => 0,
        (None, limit) => limit,
    }
}

/// Whether a target path is "occupied" — a block/char device, or a non-empty
/// regular file. A missing target is not occupied.
fn target_occupied(path: &Path) -> Result<bool> {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_block_device() || ft.is_char_device() {
                Ok(true)
            } else if ft.is_file() {
                Ok(meta.len() > 0)
            } else {
                Ok(true)
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::io(
            format!("inspecting target '{}'", path.display()),
            e,
        )),
    }
}

fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

extern "C" fn on_signal(_sig: i32) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Install a handler for SIGINT and SIGTERM that flags the copy loop to stop
/// and flush its state.
fn install_signal_handler() -> Result<()> {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};

    let action = SigAction::new(
        SigHandler::Handler(on_signal),
        SaFlags::empty(),
        SigSet::empty(),
    );
    // SAFETY: `on_signal` only performs a single atomic store, which is
    // async-signal-safe; the SigAction outlives the call.
    unsafe {
        sigaction(Signal::SIGINT, &action).map_err(|e| Error::Signal(e.to_string()))?;
        sigaction(Signal::SIGTERM, &action).map_err(|e| Error::Signal(e.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{default_state_path, domain_length};
    use std::path::{Path, PathBuf};

    #[test]
    fn default_state_path_appends_extension() {
        assert_eq!(
            default_state_path(Path::new("grave.img")),
            PathBuf::from("grave.img.state")
        );
    }

    #[test]
    fn domain_respects_skip_and_count() {
        // Whole source.
        assert_eq!(domain_length(Some(1000), 0, 0), 1000);
        // Skip reduces the domain.
        assert_eq!(domain_length(Some(1000), 200, 0), 800);
        // Count caps it.
        assert_eq!(domain_length(Some(1000), 0, 100), 100);
        assert_eq!(domain_length(Some(1000), 200, 100), 100);
        // Count larger than what is left is clamped by the source.
        assert_eq!(domain_length(Some(1000), 950, 100), 50);
        // Unknown size: only count bounds it.
        assert_eq!(domain_length(None, 0, 0), 0);
        assert_eq!(domain_length(None, 0, 100), 100);
    }
}
