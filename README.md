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
medium to death — and hands over cleanly: `--export-map` writes the region map
as a ddrescue mapfile, so ddrescue picks up exactly where exhume left off,
skipping everything already copied.

## Usage

```
exhume [OPTIONS] <SOURCE> [TARGET] [STATE]
```

| Argument | Meaning                                                    | Default            |
| -------- | ---------------------------------------------------------- | ------------------ |
| `SOURCE` | Block device or file to read from                          | (required)         |
| `TARGET` | Block device or file to write to                           | `grave.img`        |
| `STATE`  | TOML state file recording parameters and progress          | `<TARGET>.state` (for a device target: `<basename>.state` in the current directory) |

Options:

- `--sector-size <SIZE>` — logical sector size: the alignment and recovery
  granularity. Auto-detected from block devices (falls back to `512`); override
  only if you must. Accepts `512`, `64K`, `1M`, `1.5G`, `4KiB`, `1MB`, …
- `-t, --transfer-size <SIZE>` — I/O size for healthy reads (default `1M`),
  aligned down to a whole number of sectors. Also the write granularity of a
  refresh's changed chunks.
- `-l, --length <SIZE>` — copy at most this many bytes (`0` = whole source)
- `--skip <SIZE>` — skip this many bytes at the start of the source. Must be a
  whole number of sectors; a misaligned value is rejected with the nearest
  aligned values suggested.
- `--seek <SIZE>` — seek this many bytes into the target before writing. Must be
  a whole number of sectors, as for `--skip`.
- `--refresh` — make the target match the source with minimal I/O: chunks
  whose hash still matches the manifest are skipped entirely (no target read,
  no write); everything else is compared against the target block-wise and
  only differing blocks are written. Works with or without an existing state
  (without one it compares against the target throughout and records a
  manifest for next time), and implies consent to write the occupied target —
  no `--force` needed. Run `--verify` periodically; whatever it finds, the
  next refresh repairs.
- `--skip-zeros` — don't write all-zero source blocks, keeping the target
  sparse (like `dd conv=sparse`). No target read needed. For first-time
  imaging onto a fresh/zeroed target: on an *existing* target the old bytes
  would simply remain, which is why it conflicts with `--refresh` (exhume
  also warns on an occupied target). A regular-file target is always extended
  to the full size at the end (as a sparse hole) even if the trailing blocks
  were skipped.
- `--hash-chunk <SIZE>` — chunk size of the hash manifest (default `64M`).
  Must be a multiple of the sector size; the grid is fixed once recorded.
- `--verify` — after the copy, read the target back and check it against the
  manifest; mismatching chunk offsets are reported and recorded, and the exit
  code is `3`. Re-running a completed command with `--verify` copies nothing
  and just verifies — e.g. months later, to check an archived image for
  bit-rot. Also works one-shot (`exhume src dst --verify`).
- `--retry` — re-read regions marked `bad` in a previous run (resume) and
  recover what is now readable, at sector granularity. One pass per invocation;
  re-run for more.
- `--export-map <PATH>` — after the run, also write the region map as a GNU
  ddrescue mapfile (`+` done, `-` bad, `?` untried), for escalating an
  unfinished rescue: `ddrescue <source> <target> <PATH>` continues where exhume
  stopped. Positions are absolute source offsets (`--skip` included); with a
  nonzero `--seek` you need matching offset options on the ddrescue side. To
  export from an existing state without copying anything new, just re-run the
  same (already completed or bad-region-only) command with `--export-map`.
- `--direct` — read the source bypassing the page cache (`O_DIRECT` on Linux,
  `F_NOCACHE` on macOS), so a re-read (e.g. under `--retry`) actually hits the
  medium instead of returning cached bytes. Reads only — the target is written
  normally. Primarily for failing devices. If the source's filesystem rejects
  `O_DIRECT`, exhume says so and you can retry without it.
- `--status` — show the state (progress, bad regions, manifest coverage)
  without copying anything. Works with the usual arguments (`exhume /dev/sdb
  backup.img --status`) or pointed straight at a state file (`exhume --status
  run.state`). With `--json`, machine-readable.
- `-f, --force` — overwrite an existing, non-empty target
- `--allow-mounted` — proceed even if the target device (or a partition /
  stacked device on it) is mounted. Dangerous: writing under a live filesystem
  corrupts it; this is deliberately separate from `--force`
- `-q, --quiet` — suppress the progress bar
- `--json` — print the final summary as a single JSON object on stdout instead
  of human-readable text (the progress bar still draws to stderr). The object
  carries `status` (`completed` / `interrupted` / `errors`), the source/target/
  state paths, the byte and bad-region totals, and a `verify` object when a
  verify pass ran
- `--json-progress` — emit NDJSON progress events on stdout (one line every
  ~2 s per phase, `"phase": "copy"` / `"verify"`), followed by the final
  summary as a compact JSON line — for cron jobs, GUIs, and orchestration
  that would otherwise have to scrape the progress bar off stderr
- `-v, --verbose` — increase log verbosity (`-v`, `-vv`, `-vvv`)

## Safety

exhume refuses the classic `dd` footguns before a single byte is written:

- Writing to an existing block device or a non-empty file requires `--force`.
  Two exceptions express the same informed intent: a **resume** (a matching
  state file already exists) and a **refresh** (`--refresh` says "make this
  target match this source"). Both proceed without `--force`.
- A **mounted target** device — itself, one of its partitions, or a stacked
  device on it (LVM, dm-crypt, MD, active swap) — is refused even with
  `--force`; only the explicit `--allow-mounted` overrides it. A mounted
  *source* just warns: the image may be inconsistent.
- **Source == target** (also aliased through symlinks or hardlinks) is refused.
- A block-device target **too small** for the copy is refused up front, not
  discovered as a write error at the capacity boundary hours in.
- A resumed **state file must match the command line**: a different target, or
  conflicting `--skip` / `--seek` / `--sector-size` values, abort the run
  instead of silently reusing a region map that no longer applies.

Sources must be block devices or (character-device / regular) files; pipes and
sockets are rejected with a pointer to `cat`/`dd` — exhume reads by offset and
deliberately stays out of the streaming business.

## Integrity

Every run records an **integrity manifest** in the state file: a BLAKE3
digest per 64 MiB chunk, computed on the fly while copying — the hashing
itself is effectively free, the copy stays I/O-bound. Chunks the copy could
not stream in one piece (resume seams, `--retry` recoveries) are hashed from
the target at the end of the run, so any error-free run leaves a complete
manifest.

`--verify` reads the target back and compares it against the manifest:

```sh
exhume /dev/sdb backup.img backup.state           # image + manifest
exhume /dev/sdb backup.img backup.state --verify  # …and read-back verification
# months later — no source needed, nothing is copied:
exhume /dev/sdb backup.img backup.state --verify  # bit-rot check, exit 3 on mismatch
# nightly: re-scan the source, touch only what changed:
exhume /dev/sdb backup.img backup.state --refresh
```

Verification needs only the target and the state file; the source is never
read. A mismatch names the chunk offsets, so you know *where* an archived
image went bad, not just *that* it did.

Verify is **resumable**: progress is checkpointed to the state file's
`[verify]` section, so an interrupted pass over a many-TB image continues at
its cursor when you re-run with `--verify`. A completed pass records its
result and timestamp — `exhume --status run.state` shows it as a "last
verified" line. Any write to the target (a resumed copy, a retry recovery)
drops the recorded result, since it no longer describes the target.

Refresh and verify close a loop: **`--refresh` trusts the manifest** (that is
where its speed comes from), so target-side rot in chunks whose *source*
never changed is invisible to it — but a periodic `--verify` records exactly
those chunks, and the **next `--refresh` rewrites them**. Verify finds,
refresh repairs.

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

If you don't pass a `STATE` argument, exhume derives `<TARGET>.state` — except
for a device target, where the state goes to `<basename>.state` in the current
directory instead (e.g. `./sdb.state` when restoring to `/dev/sdb`): `/dev` is
devtmpfs, so a state file there would not survive the reboot that likely
interrupted the restore in the first place. The auto-named file is **removed
automatically when the copy finishes with no errors** — a clean run leaves no
clutter behind. A state file you name
explicitly is always kept, and an auto-named one is also kept if the run is
interrupted or any blocks were unreadable, so you can resume or inspect it. Note
that once a clean auto-run has removed its state, re-running the same command
sees an occupied target with no resume consent and will ask for `--force`
(or use `--refresh`, which carries its own consent).

## Status

Early days. The tool does block-wise copy with progress, a human-readable state
file, resume, sector-aware read-error handling (a failed transfer block is
isolated down to the dead sectors), an always-on integrity manifest with
`--refresh` (manifest-based image refreshing) and resumable `--verify`
closing the rot-repair loop, a `--retry` pass for `bad` regions, a `--direct`
mode so retries bypass the page cache, `--json` / `--json-progress` for
scripting, the preflight safety checks described above, and a `--export-map`
handover to GNU ddrescue.

exhume runs on Linux and, experimentally, on macOS (there, image the raw `/dev/rdiskN` node —
it is far faster than the buffered `/dev/diskN`). The macOS mounted-device
guard does not yet trace APFS-synthesized containers to their physical store,
so `diskutil unmountDisk` before imaging remains a good habit. Other Unixes
compile with safely degraded behavior (default sector size, no `--direct`, no
mounted guard).

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
