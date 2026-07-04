//! The verify pass: read the target back and check it against the manifest —
//! resumable, with the result recorded in the state file.
//!
//! Verify walks the chunk grid strictly in ascending order, so a **cursor**
//! (the next chunk index) is the entire resume state. It is checkpointed to
//! the state file's `[verify]` section on the same cadence as the copy loop's
//! map, together with the mismatches found so far; a completed pass drops the
//! cursor and records `finished` + `ok`. The section describes a snapshot of
//! the target, so any later write to the target invalidates it (the copy
//! engine drops it then).

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::Instant;

use chrono::{DateTime, Utc};
use tracing::{info, warn};

use crate::error::{Error, Result};
use crate::hash::{ChunkHasher, Digester, chunk_count};
use crate::params::RunParams;
use crate::progress::Reporter;
use crate::region::RegionMap;
use crate::state::{StateFile, VerifyState};

/// Outcome of a `--verify` pass: the target read back against the manifest.
/// The counts describe *this run*; `mismatches` describes the whole pass
/// (accumulated across resumes), which is what the exit code keys on.
#[derive(Debug, Clone)]
pub struct VerifyOutcome {
    /// Chunks checked against a recorded digest by this run.
    pub chunks_checked: u64,
    /// Bytes read back and verified by this run.
    pub bytes_verified: u64,
    /// Chunks in the grid without a digest (bad regions, incomplete manifest).
    pub chunks_unhashed: u64,
    /// Domain offsets of mismatching chunks, over the whole pass.
    pub mismatches: Vec<u64>,
    /// The pass was interrupted; the cursor is saved for a resume.
    pub interrupted: bool,
}

impl VerifyOutcome {
    /// Whether everything checked matched (an interrupted pass is not ok).
    #[must_use]
    pub fn ok(&self) -> bool {
        self.mismatches.is_empty() && !self.interrupted
    }
}

/// Everything the verify pass needs from the engine: the target handle, the
/// domain geometry, and the ingredients to persist the state file.
pub(crate) struct VerifyCtx<'a> {
    pub dst: &'a File,
    pub seek: u64,
    pub domain: u64,
    pub transfer: u64,
    pub quiet: bool,
    pub json_progress: bool,
    pub params: &'a RunParams,
    pub map: &'a RegionMap,
    pub created: DateTime<Utc>,
    pub bytes_written: u64,
    pub state_path: &'a Path,
}

impl VerifyCtx<'_> {
    /// Persist the state with the given verify section (map and manifest are
    /// final at this point — only `[verify]` still changes).
    fn save(&self, hasher: &ChunkHasher, verify: &VerifyState) -> Result<()> {
        StateFile::build(
            self.params,
            self.map,
            self.domain,
            self.created,
            self.bytes_written,
            Some(crate::engine::manifest_of(hasher)),
            Some(verify.clone()),
        )
        .save_atomic(self.state_path)
    }
}

/// Run (or resume) a verify pass. `prior` is the `[verify]` section carried
/// over from the loaded state — a cursor there continues the pass; anything
/// else starts a fresh one.
pub(crate) fn run(
    ctx: &VerifyCtx,
    hasher: &ChunkHasher,
    prior: Option<VerifyState>,
) -> Result<VerifyOutcome> {
    let chunk_size = hasher.chunk_size();
    let grid = if ctx.domain > 0 {
        chunk_count(ctx.domain, chunk_size)
    } else {
        hasher.chunks().len() as u64
    };

    let mut state = match prior {
        Some(mut state) if state.cursor.is_some() => {
            // Clamp: the grid may have shrunk since the cursor was written.
            let cursor = state.cursor.unwrap_or(0).min(grid);
            if cursor > 0 {
                info!(
                    chunk = cursor,
                    of = grid,
                    "resuming the interrupted verify pass"
                );
            }
            state.cursor = Some(cursor);
            state
        }
        _ => VerifyState {
            cursor: Some(0),
            mismatches: Vec::new(),
            started: Utc::now(),
            finished: None,
            ok: None,
        },
    };
    let first = state.cursor.unwrap_or(0);

    let bytes_total: u64 = (first..grid)
        .filter(|&i| hasher.get(i).is_some())
        .map(|i| chunk_len(i, chunk_size, ctx.domain))
        .sum();
    let reporter = Reporter::new(
        bytes_total,
        0,
        ctx.quiet,
        ctx.json_progress.then_some("verify"),
    );

    let mut outcome = VerifyOutcome {
        chunks_checked: 0,
        bytes_verified: 0,
        chunks_unhashed: 0,
        mismatches: Vec::new(),
        interrupted: false,
    };
    let mut buf = vec![0u8; usize::try_from(ctx.transfer).expect("transfer fits in usize")];
    let mut last_flush = Instant::now();

    for index in first..grid {
        if crate::engine::interrupted() {
            outcome.interrupted = true;
            state.cursor = Some(index);
            ctx.save(hasher, &state)?;
            break;
        }
        let Some(expected) = hasher.get(index) else {
            outcome.chunks_unhashed += 1;
            continue;
        };
        let start = index * chunk_size;
        let len = chunk_len(index, chunk_size, ctx.domain);
        match digest_target_range(ctx.dst, ctx.seek + start, len, &mut buf, &reporter)? {
            Some(digest) if digest == expected => {}
            _ => {
                warn!(
                    offset = start,
                    "verify mismatch — chunk differs from the manifest"
                );
                state.mismatches.push(start);
            }
        }
        outcome.chunks_checked += 1;
        outcome.bytes_verified += len;
        if last_flush.elapsed() >= crate::engine::FLUSH_INTERVAL {
            state.cursor = Some(index + 1);
            ctx.save(hasher, &state)?;
            last_flush = Instant::now();
        }
    }

    if !outcome.interrupted {
        state.cursor = None;
        state.finished = Some(Utc::now());
        state.ok = Some(state.mismatches.is_empty());
        ctx.save(hasher, &state)?;
    }
    outcome.mismatches = state.mismatches;
    reporter.finish();
    Ok(outcome)
}

/// The expected length of chunk `index`: `chunk_size`, clipped by the domain
/// tail (an unknown domain expects full chunks; its tail sorts itself out via
/// the short-read path).
fn chunk_len(index: u64, chunk_size: u64, domain: u64) -> u64 {
    let start = index * chunk_size;
    if domain > 0 {
        chunk_size.min(domain - start)
    } else {
        chunk_size
    }
}

/// Digest `len` bytes of the target at `offset`, in `buf`-sized reads. `None`
/// on a short read (truncated target).
fn digest_target_range(
    dst: &File,
    offset: u64,
    len: u64,
    buf: &mut [u8],
    reporter: &Reporter,
) -> Result<Option<String>> {
    let mut digester = Digester::new();
    let mut pos = 0u64;
    while pos < len {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "bounded by the buffer length, which fits in usize"
        )]
        let want = (buf.len() as u64).min(len - pos) as usize;
        let n = dst
            .read_at(&mut buf[..want], offset + pos)
            .map_err(|e| Error::io(format!("reading target back at offset {}", offset + pos), e))?;
        if n == 0 {
            return Ok(None);
        }
        digester.update(&buf[..n]);
        reporter.inc(n as u64);
        pos += n as u64;
    }
    Ok(Some(digester.finish()))
}
