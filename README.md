# exhume

A modern, resumable disk imaging and rescue tool — a friendlier `dd` /
`dd_rescue` with a small mental footprint.

```sh
exhume /dev/sdb                 # image a device to ./grave.img (+ grave.img.state)
exhume /dev/sdb backup.img      # image to a named file
exhume backup.img /dev/sdb      # restore an image back to a device
exhume /dev/sda /dev/sdb        # clone one device to another
```

## Why

`dd` is powerful but unforgiving: cryptic `if=/of=/bs=/conv=` syntax, no
progress by default, it aborts on the first read error unless you remember
`conv=noerror,sync`, and there is no way to resume an interrupted run. `ddrescue`
and `dd_rescue` add recovery features but carry a lot of conceptual overhead and
a terse mapfile format.

exhume aims for the middle: **sane positional arguments, safe defaults, progress
by default, and a human-readable TOML state file** that makes a run resumable by
simply re-running the same command.

**Scope.** exhume is for sunny days and light rain — imaging, refreshing, and
surviving the *occasional* read error, primarily on flash/SSD. For a dying hard
disk, hours of scraping, or heavy damage, reach for **GNU ddrescue**: it is built
for the storm. exhume deliberately stops short of that rather than read a failing
medium to death.

## Usage

```
exhume [OPTIONS] <SOURCE> [TARGET] [STATE]
```

| Argument | Meaning                                                    | Default            |
| -------- | ---------------------------------------------------------- | ------------------ |
| `SOURCE` | Block device or file to read from                          | (required)         |
| `TARGET` | Block device or file to write to                           | `grave.img`        |
| `STATE`  | TOML state file recording parameters and progress          | `<TARGET>.state`   |

Options:

- `--sector-size <SIZE>` — logical sector size: the alignment and recovery
  granularity. Auto-detected from block devices (falls back to `512`); override
  only if you must. Accepts `512`, `64K`, `1M`, `1.5G`, `4KiB`, `1MB`, …
- `-t, --transfer-size <SIZE>` — I/O size for healthy reads (default `1M`),
  aligned down to a whole number of sectors. A smaller value also makes the
  `--skip-*` modes act at a finer granularity.
- `-l, --length <SIZE>` — copy at most this many bytes (`0` = whole source)
- `--skip <SIZE>` — skip this many bytes at the start of the source. Must be a
  whole number of sectors; a misaligned value is rejected with the nearest
  aligned values suggested.
- `--seek <SIZE>` — seek this many bytes into the target before writing. Must be
  a whole number of sectors, as for `--skip`.
- `--skip-unchanged` — only write blocks that differ from the current target
  contents (reads the target block to compare first). For *refreshing* an
  existing image or clone — saves writes on SSDs/flash and keeps CoW/snapshot
  deltas small. Not useful for first-time imaging (every block differs, so you
  only pay for the extra target reads). The summary then reports bytes scanned
  vs bytes actually written.
- `--skip-zeros` — don't write all-zero source blocks, keeping the target
  sparse (like `dd conv=sparse`). No target read needed. **Caveat:** this assumes
  the target reads as zero where writes are skipped — true for a fresh/sparse
  file. On an *existing* target or block device the old bytes simply remain, so a
  region that became zero in the source is **not** cleared; use `--skip-unchanged`
  to refresh an existing target correctly. exhume warns when `--skip-zeros` is
  used on an occupied target. A regular-file target is always extended to the full
  size at the end (as a sparse hole) even if the trailing blocks were skipped.
- `--retry` — re-read regions marked `bad` in a previous run (resume) and
  recover what is now readable, at sector granularity. One pass per invocation;
  re-run for more.
- `-f, --force` — overwrite an existing, non-empty target
- `-q, --quiet` — suppress the progress bar
- `-v, --verbose` — increase log verbosity (`-v`, `-vv`, `-vvv`)

## Safety

Writing to an existing block device or a non-empty file requires `--force`. The
one exception is a **resume**: if a matching state file already exists, exhume
treats that as your intent to continue and proceeds without `--force`.

## Resuming

Progress is checkpointed to the state file periodically and on `Ctrl-C`. To
resume, run the same command again — exhume reads the state file and copies only
the regions that are still untried. Parameters recorded in the state file
(sector and transfer size, offsets, …) are reused unless you override them on
the command line, in which case the command line wins.

## State file

The state file is plain TOML and safe to read or hand-edit:

```toml
[meta]
version = 1
program = "exhume"
program_version = "0.1.0"
created = "2026-06-18T08:00:00Z"
updated = "2026-06-18T08:05:00Z"

[params]
source = "/dev/sdb"
target = "grave.img"
sector_size = 512
transfer_size = 1048576
skip = 0
seek = 0
length = 0

[progress]
bytes_total = 2000398934016
bytes_done = 1048576
bytes_written = 1048576
errors = 0

[[regions]]
start = 0
length = 1048576
status = "done"
```

`status` is one of `untried`, `done`, or `bad`.

## Status

Early days. The tool does block-wise copy with progress, a human-readable state
file, resume, sector-aware read-error handling (a failed transfer block is
isolated down to the dead sectors), the `--skip-unchanged` / `--skip-zeros`
write-reduction modes, and a `--retry` pass for `bad` regions. Planned: an
`O_DIRECT` mode so retries bypass the page cache, and a `--json` status mode.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
