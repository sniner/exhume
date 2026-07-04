# Changelog

Format based on [Keep a Changelog](https://keepachangelog.com).

## [Unreleased]

### Added

- **`--refresh`** — re-scans a completed state against its source, the mode for periodically
  refreshed images: chunks whose hash still matches the manifest are skipped without any
  target I/O; changed chunks are written block-wise with target comparison and their digests
  updated. The manifest is trusted — target-side rot in unchanged chunks is not repaired
  (pair with a periodic `--verify`); `--refresh --skip-unchanged` compares against the target
  instead and repairs rot too. A state without a manifest refreshes by target comparison and
  records a manifest on the way. The summary reports the new `bytes_skipped_by_hash` and
  `bytes_written_this_run` (both also in `--json`)

### Changed

- **`--verify` is resumable and records its result** — verify progress is checkpointed to a new
  `[verify]` section in the state file (cursor + mismatches), so an interrupted pass continues
  where it stopped on the next `--verify`. A completed pass keeps its result and timestamp in
  the state (`--status` shows it as a "Verify:" line); any write to the target drops the
  recorded result, since it no longer describes the target

## [0.3.0] - 2026-07-04

### Added

- **Integrity manifest (`[hashes]`)** — with hashing on, exhume records a BLAKE3 digest per
  fixed-size chunk (default 64 MiB, `--hash-chunk` to change) in the state file, computed on
  the fly from the source bytes as they are copied; chunks broken by resume seams or `--retry`
  recoveries are filled in by reading them back from the target at the end of the run. Hashing
  is on when STATE is named explicitly (`--hash` forces it for an auto-named state,
  `--hash=false` switches it off). The manifest is the basis for `--verify`
- **`--verify`** — reads the target back and checks it against the hash manifest, reporting
  the offsets of mismatching chunks; exit code `3` on mismatch. Re-running a completed command
  with `--verify` copies nothing and just verifies — a bit-rot check for archived images that
  needs neither the source nor a byte of copying. The `--json` summary carries the result in a
  `verify` object
- **`--status`** — renders a state file for humans without copying: progress with percentage,
  untried/bad totals, the bad-region list, and manifest coverage. Takes the usual arguments or
  the state file directly (`exhume --status run.state`); `--json` gives the same as JSON
- **`--json-progress`** — emits NDJSON progress events on stdout (one line every ~2 s, with
  phase, bytes done/total, and throughput), followed by the final summary as a compact JSON
  line, so scripts and GUIs no longer have to scrape the progress bar off stderr

### Changed

- **No-op resumes say so** — re-running an already completed command now reports
  "Already complete — … nothing copied this run." instead of a misleading "Done — copied …";
  the `--json` summary carries the new `bytes_done_this_run` field

## [0.2.0] - 2026-07-04

Includes the changes of the unpublished 0.1.1.

### Added

- **Preflight safety checks** — exhume now refuses, before writing anything: a target device
  that is mounted (itself, a partition, or a stacked LVM/dm-crypt/MD device on it, including
  active swap) unless the new `--allow-mounted` is passed; a source and target that are the
  same file (also via symlink/hardlink aliases); and a block-device target too small to hold
  the copy. A mounted *source* warns that the image may be inconsistent
- **`--export-map <PATH>`** — writes the final region map as a GNU ddrescue mapfile (`+` done,
  `-` bad, `?` untried; positions are absolute source offsets), so an unfinished rescue can be
  handed over to ddrescue exactly where exhume left off, skipping everything already copied
- **`--skip-unchanged` / `--skip-zeros` take an optional value** — the modes are sticky across
  resumes, and `--skip-unchanged=false` / `--skip-zeros=false` now switch them off again
  without editing the state file

### Changed

- **Streams are rejected up front** — a pipe (FIFO) or socket source now fails immediately with
  a clear message; exhume reads by offset, so streams never actually worked (they died mid-run
  with "Illegal seek") and staying out of the streaming business is deliberate — use `cat` or
  `dd` for those
- **Ctrl-C** — the first interrupt now announces on stderr that exhume is finishing the current
  block and saving state; a second Ctrl-C (or SIGTERM) aborts immediately — useful when a read
  hangs on failing hardware
- **Device targets keep their auto-named state file in the current directory** (`./sdb.state`
  for `/dev/sdb`) instead of deriving `/dev/sdb.state` on devtmpfs, which is often not writable
  and never survives a reboot — exactly when an interrupted restore needs the state to resume.
  An explicitly named state file is unaffected

### Fixed

- **Resume** — a state file recorded for a different target, or `--skip` / `--seek` /
  `--sector-size` values that conflict with the resumed state, are now refused with a clear
  message instead of silently reusing the old region map (previously a state file from another
  target could produce an all-zero target reported as "Done")
- **Resume** — the region map is reconciled with the current copy domain: resuming with a larger
  `--length` (or a grown source) now copies the new tail instead of reporting early completion,
  and a smaller `--length` no longer copies past the limit
- **State file** — checkpoints are synced to disk before the atomic rename, so a power loss or
  kernel panic right after a checkpoint can no longer leave an empty state file behind
- **Read errors are classified** — only genuine media errors (`EIO`, `EREMOTEIO`, `EBADMSG`)
  mark sectors as `bad`; anything else (an `O_DIRECT` alignment problem, a vanished device, …)
  now aborts the run instead of silently sweeping the rest of the source into `bad`
- **`--direct`** refuses a non-power-of-two `--sector-size` up front, instead of failing every
  read with `EINVAL`
- **State is checkpointed on fatal errors** — a run that dies on a write error (or any other
  fatal condition) now flushes the state file first, so the progress since the last periodic
  checkpoint survives

## [0.1.0] - 2026-06-29

### Added

- Initial release: `exhume <SOURCE> [TARGET] [STATE]` copies a block device or
  file to a target block-wise, defaulting the target to `grave.img` and the
  state file to `<TARGET>.state`
- **State file** — a human-readable TOML file recording the full run
  configuration (`[params]`), progress totals (`[progress]`), and a region map
  (`[[regions]]` with `untried` / `done` / `bad` status). Written atomically and
  checkpointed periodically and on `Ctrl-C`
- **Resume** — re-running the same command continues an interrupted copy, reading
  only still-untried regions; recorded parameters are reused unless overridden on
  the command line
- **Read-error tolerance** — read errors are recorded as `bad` regions and skipped
  instead of aborting the copy. A failed transfer block is re-read sector by
  sector so only the unreadable part is lost, not the whole block: capped at one
  page on a buffered source (read-ahead is switched off on the first error), and
  down to a single sector with `--direct`
- **`--skip-unchanged`** — reads each target block and writes only the ones that
  differ from the source, for refreshing an existing image/clone with minimal
  writes; the summary and state file report bytes written separately from bytes
  scanned (`progress.bytes_written`)
- **`--skip-zeros`** — leaves all-zero source blocks unwritten to keep the target
  sparse (like `dd conv=sparse`), without reading the target. A regular-file
  target is extended to the full size at the end so skipped trailing zeros still
  produce a correctly sized, sparse image. Warns when used on an occupied target,
  where pre-existing data would survive. Combinable with `--skip-unchanged`
- **`--retry`** — re-reads regions previously recorded as `bad` and recovers
  what is now readable, flipping them to `done`, at sector granularity. One pass
  per run; not sticky (each retry is opt-in, to avoid hammering failing media on
  every resume)
- **`--direct`** — reads the source with `O_DIRECT`, bypassing the page cache so
  a re-read (e.g. under `--retry`) actually reaches the medium instead of being
  served stale from cache. Reads only; Linux only; opt-in, for failing media
- **Overwrite safety** — writing to an existing block device or non-empty file
  requires `--force`; an existing matching state file enables resume instead
- **Auto-named state files are cleaned up** — when no `STATE` argument is given,
  the derived `<TARGET>.state` is removed after a copy that finishes with no
  errors, so a clean run leaves nothing behind (including on block-device
  targets). An explicitly named state file, or one left by an interrupted or
  error-hitting run, is kept for resume/inspection
- **Sector-aware copy** — the logical sector size is auto-detected from block
  devices (`BLKSSZGET`, falling back to 512 bytes) and used as the alignment and
  recovery granularity; healthy reads use a larger `--transfer-size` (default
  `1M`) aligned down to it. `--sector-size` overrides the detection. `--skip` and
  `--seek` must be whole sectors and are rejected up front otherwise, with the
  nearest aligned values suggested
- Size arguments (`--sector-size`, `--transfer-size`, `--length`, `--skip`,
  `--seek`) accept human-readable suffixes (`64K`, `1M`, `1.5G`, `4KiB`, `1MB`)
- Progress bar with throughput and ETA (suppressible with `--quiet`)
- **`--json`** — prints the final run summary as a single JSON object on stdout
  (status, paths, and byte/bad-region totals) for scripting; the progress bar
  still draws to stderr
