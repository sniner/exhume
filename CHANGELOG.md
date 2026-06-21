# Changelog

Format based on [Keep a Changelog](https://keepachangelog.com).

## [Unreleased]

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
  instead of aborting the copy
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
  what is now readable, flipping them to `done`. `--retry-block-size` reads them
  in smaller chunks to salvage the readable part of a partially-bad block. One
  pass per run; not sticky (each retry is opt-in, to avoid hammering failing
  media on every resume)
- **Overwrite safety** — writing to an existing block device or non-empty file
  requires `--force`; an existing matching state file enables resume instead
- Size arguments (`--block-size`, `--count`, `--skip`, `--seek`) accept
  human-readable suffixes (`64K`, `1M`, `1.5G`, `4KiB`, `1MB`)
- Progress bar with throughput and ETA (suppressible with `--quiet`)
- **`--json`** — prints the final run summary as a single JSON object on stdout
  (status, paths, and byte/bad-region totals) for scripting; the progress bar
  still draws to stderr
