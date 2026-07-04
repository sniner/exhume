//! The copy engine: parameter resolution, overwrite safety, size detection,
//! the block-wise copy loop, and graceful interruption.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Seek, SeekFrom};
use std::ops::{Deref, DerefMut};
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

    // Checked on the path, before the open: opening a FIFO would block forever.
    crate::safety::ensure_source_supported(&cli.source)?;

    // Open the source early: the sector size is probed from it and feeds
    // parameter resolution.
    let mut src = File::open(&cli.source)
        .map_err(|e| Error::io(format!("opening source '{}'", cli.source.display()), e))?;
    let detected_sector = detect_sector_size(&src);

    let params = resolve_params(cli, target.clone(), existing.as_ref(), detected_sector)?;

    // Effective transfer size: the desired size aligned down to whole sectors,
    // never below one sector.
    let transfer = align_down(params.transfer_size, params.sector_size).max(params.sector_size);

    let occupied = guard_target(cli, &target, resuming)?;

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

    // With source size and target capacity both known, a too-small target can
    // fail now instead of at the capacity boundary hours in.
    crate::safety::ensure_target_capacity(&dst, &target, params.seek, domain)?;

    let mut map = match &existing {
        Some(state) => state.region_map(),
        None => RegionMap::from_total(domain),
    };
    // A resumed map may describe a different domain (changed --length, resized
    // source): mark the uncovered remainder untried and drop the out-of-domain
    // overhang, so neither is silently mishandled.
    map.reconcile(domain);

    let created = existing.as_ref().map_or_else(Utc::now, |s| s.meta.created);

    let processed_start = domain.saturating_sub(map.bytes_with(RegionStatus::Untried));
    let reporter = Reporter::new(domain, processed_start, cli.quiet);
    let prior_written = existing.as_ref().map_or(0, |s| s.progress.bytes_written);

    let direct_src = setup_direct(cli, &params, domain)?;
    let direct = direct_src.is_some();
    let read_src = direct_src.as_ref().unwrap_or(&src);

    let mut copier = Copier::new(
        read_src,
        &dst,
        &params,
        &reporter,
        &state_path,
        created,
        domain,
        map,
        prior_written,
        transfer,
        direct,
    );
    let interrupted = match copier.drive(cli.retry) {
        Ok(interrupted) => interrupted,
        // A fatal error must not lose the progress since the last periodic
        // checkpoint — flush best-effort, then report the real problem.
        Err(e) => {
            let _ = copier.flush();
            return Err(e);
        }
    };

    // Persist the final state regardless of how the loop ended.
    copier.flush()?;
    let (map, bytes_written) = copier.into_parts();
    reporter.finish();

    // Materialise a sparse tail: ensure a regular-file target spans the whole
    // processed domain even if trailing blocks were skipped (zero or unchanged).
    ensure_len(&dst, params.seek + map.covered_end())?;

    // Handover to the heavy machinery: the final map as a ddrescue mapfile.
    if let Some(map_path) = &cli.export_map {
        crate::mapfile::export(&map, params.skip, map_path)?;
        info!(mapfile = %map_path.display(), "wrote ddrescue mapfile");
    }

    let summary = summarize(
        params,
        target,
        state_path,
        domain,
        &map,
        bytes_written,
        interrupted,
    );
    discard_auto_state(cli.state.is_some(), &summary);
    Ok(summary)
}

/// Set up the `O_DIRECT` read side (reads only): a separate fd opened with
/// `O_DIRECT`, used for the copy so re-reads bypass the page cache and hit the
/// medium. `None` (with a warning) where it does not apply — a non-seekable
/// source, or off Linux.
fn setup_direct(cli: &Cli, params: &RunParams, domain: u64) -> Result<Option<File>> {
    if !cli.direct {
        return Ok(None);
    }
    if domain == 0 {
        warn!("--direct ignored: the source is not seekable");
        return Ok(None);
    }
    if !cfg!(target_os = "linux") {
        warn!("--direct ignored: O_DIRECT is only available on Linux");
        return Ok(None);
    }
    // O_DIRECT wants offsets, lengths, and the buffer aligned to the device's
    // block granularity; a stray sector size would make every read fail with
    // EINVAL (which is fatal, not a bad sector), so refuse it up front.
    if !params.sector_size.is_power_of_two() {
        return Err(Error::InvalidSize(format!(
            "--direct requires a power-of-two sector size (O_DIRECT alignment), \
             but the effective sector size is {}",
            params.sector_size
        )));
    }
    let f = open_source_direct(&cli.source)?;
    verify_direct(&f, params.sector_size, params.skip)?;
    Ok(Some(f))
}

/// The write-side guards, run before the target is opened: refuse to copy a
/// file onto itself; an existing, occupied target needs `--force`, unless a
/// state file is present (which signals an intentional resume); a mounted
/// target device is refused outright (`--force` does not cover corrupting a
/// live filesystem). Returns whether the target was occupied, which the
/// `--skip-zeros` warning needs later.
fn guard_target(cli: &Cli, target: &Path, resuming: bool) -> Result<bool> {
    crate::safety::ensure_not_same_file(&cli.source, target)?;
    let occupied = target_occupied(target)?;
    if !resuming && !cli.force && occupied {
        return Err(Error::TargetExists(target.to_path_buf()));
    }
    crate::safety::check_mounted(&cli.source, target, cli.allow_mounted)?;
    Ok(occupied)
}

/// Remove an auto-named state file after a clean, error-free copy: it is just
/// scaffolding, with nothing left to resume or inspect. A state file the user
/// named explicitly is always kept. Best-effort — a failure is logged, not fatal.
fn discard_auto_state(state_explicit: bool, summary: &Summary) {
    if state_explicit || !summary.completed {
        return;
    }
    match std::fs::remove_file(&summary.state_path) {
        Ok(()) => info!(
            state = %summary.state_path.display(),
            "removed the auto-created state file (copy completed with no errors)"
        ),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => warn!(
            state = %summary.state_path.display(),
            error = %e,
            "could not remove the auto-created state file"
        ),
    }
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
        validate_resume(cli, &params, &prev.params, detected_sector)?;
    }

    // Offsets must sit on the sector grid (the map stays aligned; `O_DIRECT` later
    // depends on it). `length` is exempt — its tail rounds up.
    require_sector_aligned("skip", params.skip, params.sector_size)?;
    require_sector_aligned("seek", params.seek, params.sector_size)?;

    Ok(params)
}

/// Check that a resumed state file describes the same copy as the command
/// line. The region map's coordinates are relative to `skip`/`seek` on the
/// recorded target and aligned to the recorded sector grid — reusing it under
/// different values would silently skip or misplace data (e.g. a state file
/// from another target marks everything `done`, producing an all-zero "copy"
/// reported as complete). Conflicts are refused; only the source path may
/// legitimately differ (a device can be renumbered across reboots), which
/// [`run`] warns about separately.
fn validate_resume(
    cli: &Cli,
    params: &RunParams,
    prev: &RunParams,
    detected_sector: u64,
) -> Result<()> {
    if prev.target != params.target {
        return Err(Error::StateConflict(format!(
            "the state file records target '{}' but the current target is '{}'; \
             use the recorded target or a different (or no) state file",
            prev.target.display(),
            params.target.display()
        )));
    }
    if let Some(cli_sector) = cli.sector_size {
        if cli_sector != prev.sector_size {
            return Err(Error::StateConflict(format!(
                "--sector-size {} conflicts with the resumed state's sector size {} \
                 (the region map is aligned to the latter; edit or remove the state file \
                 to change it)",
                cli_sector, prev.sector_size
            )));
        }
    } else if detected_sector != prev.sector_size {
        warn!(
            detected = detected_sector,
            recorded = prev.sector_size,
            "detected sector size differs from the state file — keeping the recorded value"
        );
    }
    for (label, cli_value, recorded) in
        [("skip", cli.skip, prev.skip), ("seek", cli.seek, prev.seek)]
    {
        if let Some(value) = cli_value {
            if value != recorded {
                return Err(Error::StateConflict(format!(
                    "--{label} {value} conflicts with the resumed state's {label} {recorded} \
                     (the region map is relative to it; remove the state file to start over)"
                )));
            }
        }
    }
    Ok(())
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
    /// Read source reads with `O_DIRECT` (bypass the page cache). Requires
    /// sector-aligned offsets and lengths, satisfied by the sector model.
    direct: bool,
    /// Set once the first read error has switched off read-ahead on a buffered
    /// source, so isolation re-reads aren't poisoned beyond the failing page.
    readahead_off: bool,
    buf: AlignedBuf,
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
        direct: bool,
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
            direct,
            readahead_off: false,
            buf: AlignedBuf::new(buf_size, params.sector_size as usize),
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

    /// Read up to `want` bytes of the source at `src_off` into the buffer and
    /// return the number of usable bytes (always `<= want`). The single read
    /// path for all callers. With `O_DIRECT` the request is rounded up to a whole
    /// sector (the offset is already sector-aligned), so a short read at the
    /// file/domain tail is normal and is simply capped back to `want`.
    fn read_block(&mut self, src_off: u64, want: u64) -> std::io::Result<usize> {
        let req = if self.direct {
            align_up(want, self.params.sector_size).min(self.buf.len() as u64)
        } else {
            want
        };
        let n = self.src.read_at(&mut self.buf[..req as usize], src_off)?;
        Ok((n as u64).min(want) as usize)
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
                match self.read_block(src_off, want) {
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
                    Err(e) if is_media_error(&e) => {
                        warn!(offset = src_off, len = want, error = %e, "read error — isolating bad sectors");
                        if self.isolate(pos, want, advance)? {
                            return Ok(true);
                        }
                        pos += want;
                    }
                    // Everything else (EINVAL from O_DIRECT misuse, a vanished
                    // device, …) is not damage at this position — aborting
                    // beats sweeping the rest of the source into `bad`.
                    Err(e) => {
                        return Err(Error::io(format!("reading at source offset {src_off}"), e));
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
        // First read error on a buffered source: switch off read-ahead so the
        // sector-by-sector re-reads below aren't poisoned past the failing page.
        // (O_DIRECT already bypasses the cache, so it needs none of this.)
        if !self.direct && !self.readahead_off {
            disable_readahead(self.src);
            self.readahead_off = true;
        }
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
            match self.read_block(src_off, want) {
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
                Err(e) if is_media_error(&e) => {
                    self.map.mark(pos, want, RegionStatus::Bad);
                    if advance {
                        self.reporter.inc(want);
                    }
                    bad += want;
                    pos += want;
                }
                // Same rationale as in `process`: a non-media error aborts.
                Err(e) => {
                    return Err(Error::io(format!("reading at source offset {src_off}"), e));
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
                .read_block(src_off, want)
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

/// Whether a read error means "this spot of the medium is unreadable" — the
/// only condition under which sectors are marked `bad` and the copy moves on.
/// `EIO` is the block layer's damage signal, `EREMOTEIO` its USB-bridge
/// cousin, `EBADMSG` an integrity (T10 DIF) failure. Anything else — `EINVAL`
/// from `O_DIRECT` misuse, a vanished device, a bad fd — is not positional
/// damage, and skipping past it would sweep the rest of the source into `bad`.
fn is_media_error(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(nix::libc::EIO | nix::libc::EREMOTEIO | nix::libc::EBADMSG)
    )
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

/// Derive the default state-file path: `<target>.state` next to a regular
/// file. For a device target, `<basename>.state` in the current directory
/// instead — `/dev` is devtmpfs, where a state file would vanish on reboot,
/// which is exactly when an interrupted restore needs it.
fn default_state_path(target: &Path) -> PathBuf {
    let is_device = std::fs::metadata(target)
        .is_ok_and(|m| m.file_type().is_block_device() || m.file_type().is_char_device());
    if is_device {
        if let Some(name) = target.file_name() {
            let mut s = name.to_owned();
            s.push(".state");
            return PathBuf::from(s);
        }
    }
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

/// Round `value` up to a multiple of `align`. `align == 0` leaves it unchanged.
fn align_up(value: u64, align: u64) -> u64 {
    if align == 0 {
        value
    } else {
        value.div_ceil(align) * align
    }
}

/// A heap buffer whose usable slice starts at an address aligned to `align`
/// bytes — the requirement for `O_DIRECT` reads. No `unsafe`: it over-allocates by
/// `align` and hands out an aligned sub-slice. `align` must be a power of two
/// (sector sizes always are), and is expected to divide the eventual reads.
struct AlignedBuf {
    raw: Vec<u8>,
    offset: usize,
    len: usize,
}

impl AlignedBuf {
    fn new(len: usize, align: usize) -> Self {
        // Over-allocate by `align` so an aligned start always exists inside.
        // The Vec never grows afterwards, so this address stays valid.
        let raw = vec![0u8; len + align];
        let offset = (align - raw.as_ptr().addr() % align) % align;
        AlignedBuf { raw, offset, len }
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.raw[self.offset..self.offset + self.len]
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.raw[self.offset..self.offset + self.len]
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

/// Open the source with `O_DIRECT` so copy reads bypass the page cache (Linux).
#[cfg(target_os = "linux")]
fn open_source_direct(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECT)
        .open(path)
        .map_err(|e| {
            Error::io(
                format!("opening source '{}' with O_DIRECT", path.display()),
                e,
            )
        })
}

#[cfg(not(target_os = "linux"))]
fn open_source_direct(_path: &Path) -> Result<File> {
    unreachable!("open_source_direct is only reached when O_DIRECT is available (Linux)")
}

/// Probe that `O_DIRECT` actually works on this source with one aligned sector
/// read. A genuine I/O error (or EOF) is fine — it means the medium is just
/// unreadable there; only `EINVAL` means the source/filesystem rejects `O_DIRECT`,
/// which we turn into a clear "retry without --direct" message.
fn verify_direct(file: &File, sector: u64, offset: u64) -> Result<()> {
    let len = usize::try_from(sector).expect("sector size fits in usize");
    let mut probe = AlignedBuf::new(len, len);
    match file.read_at(&mut probe[..], offset) {
        Err(e) if e.raw_os_error() == Some(nix::libc::EINVAL) => Err(Error::io(
            "--direct is not supported for this source or filesystem; retry without it",
            e,
        )),
        _ => Ok(()),
    }
}

/// Hint the kernel to stop reading ahead on the source. Without this, a buffered
/// read near a bad sector pulls a wider read-ahead window into one failing bio,
/// ballooning a single bad byte into a much larger `bad` region; turning it off
/// caps the loss at one page. Advisory — the result is ignored. No-op off Linux.
#[cfg(target_os = "linux")]
fn disable_readahead(file: &File) {
    use std::os::unix::io::AsRawFd;
    // SAFETY: posix_fadvise on a valid fd with constant arguments; its return is
    // advisory and intentionally ignored (read-ahead simply stays on if it fails).
    unsafe {
        nix::libc::posix_fadvise(file.as_raw_fd(), 0, 0, nix::libc::POSIX_FADV_RANDOM);
    }
}

#[cfg(not(target_os = "linux"))]
fn disable_readahead(_file: &File) {}

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
    // The loop only notices the flag between block reads, and a read on dying
    // hardware can hang for minutes — without a note the graceful stop looks
    // like a freeze. `SA_RESETHAND` has already restored the default
    // disposition, so a second signal aborts hard.
    const MSG: &[u8] =
        b"\nexhume: interrupted - finishing the current block and saving state (repeat to abort hard)\n";
    INTERRUPTED.store(true, Ordering::SeqCst);
    // SAFETY: write(2) to stderr with a static buffer is async-signal-safe;
    // the result is intentionally ignored (the message is best-effort).
    unsafe {
        nix::libc::write(2, MSG.as_ptr().cast(), MSG.len());
    }
}

/// Install a one-shot handler for SIGINT and SIGTERM that flags the copy loop
/// to stop and flush its state. `SA_RESETHAND` makes it one-shot: the second
/// Ctrl-C (or SIGTERM) gets the default disposition and kills the process even
/// if the copy is stuck in an unresponsive read.
fn install_signal_handler() -> Result<()> {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};

    let action = SigAction::new(
        SigHandler::Handler(on_signal),
        SaFlags::SA_RESETHAND,
        SigSet::empty(),
    );
    // SAFETY: `on_signal` only performs an atomic store and a write(2), both
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
        // A missing target (fresh image) behaves like a file path.
        assert_eq!(
            default_state_path(Path::new("/nonexistent/dir/new.img")),
            PathBuf::from("/nonexistent/dir/new.img.state")
        );
    }

    #[test]
    fn default_state_path_for_a_device_lands_in_the_cwd() {
        // /dev/null is a character device on every Linux system; the state
        // must not be derived onto devtmpfs, where it dies with the next boot.
        assert_eq!(
            default_state_path(Path::new("/dev/null")),
            PathBuf::from("null.state")
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
