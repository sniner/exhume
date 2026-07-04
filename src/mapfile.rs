//! Export the region map as a GNU ddrescue mapfile.
//!
//! exhume is for sunny days and light rain; ddrescue is built for the storm.
//! This module makes the handover real: `--export-map` writes the state's
//! region map in ddrescue's mapfile format, so `ddrescue <source> <target>
//! <mapfile>` picks up exactly where exhume left off — everything already
//! copied is skipped, the bad and untried parts are rescued with the heavy
//! machinery.
//!
//! Status mapping: `untried` → `?` (non-tried), `done` → `+` (finished),
//! `bad` → `-` (bad sector; exhume isolates read errors down to sectors, which
//! is what ddrescue's `-` means — coarser failures would be `*`/`/`).
//! Positions are absolute source offsets, i.e. domain position plus `skip`.

use std::path::Path;

use crate::error::{Error, Result};
use crate::region::{RegionMap, RegionStatus};

/// Write `map` as a GNU ddrescue mapfile to `path`. `skip` is the domain's
/// offset into the source, added to every position.
///
/// # Errors
///
/// Returns an error if the file cannot be written.
pub fn export(map: &RegionMap, skip: u64, path: &Path) -> Result<()> {
    std::fs::write(path, render(map, skip))
        .map_err(|e| Error::io(format!("writing ddrescue mapfile '{}'", path.display()), e))
}

/// Render the mapfile text: a provenance comment, the `current_pos` line
/// (where a rescuer should continue: the first position that is not `done`),
/// and one extent line per region.
fn render(map: &RegionMap, skip: u64) -> String {
    use std::fmt::Write;

    let current = map
        .regions()
        .iter()
        .find(|r| r.status != RegionStatus::Done)
        .map_or_else(|| map.covered_end(), |r| r.start);
    let finished =
        map.bytes_with(RegionStatus::Untried) == 0 && map.bytes_with(RegionStatus::Bad) == 0;

    let mut out = format!(
        "# Mapfile. Created by {} {}\n\
         # current_pos  current_status  current_pass\n\
         0x{:08X}     {}               1\n\
         #      pos        size  status\n",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        current + skip,
        if finished { '+' } else { '?' },
    );
    for r in map.regions() {
        // Writing to a String is infallible.
        let _ = writeln!(
            out,
            "0x{:08X}  0x{:08X}  {}",
            r.start + skip,
            r.length,
            status_char(r.status)
        );
    }
    out
}

/// The ddrescue status character for a region status.
fn status_char(status: RegionStatus) -> char {
    match status {
        RegionStatus::Untried => '?',
        RegionStatus::Done => '+',
        RegionStatus::Bad => '-',
    }
}

#[cfg(test)]
mod tests {
    use super::render;
    use crate::region::{RegionMap, RegionStatus};

    #[test]
    fn renders_a_partial_rescue() {
        let mut map = RegionMap::from_total(0x4000);
        map.mark(0, 0x1000, RegionStatus::Done);
        map.mark(0x1000, 0x200, RegionStatus::Bad);
        map.mark(0x1200, 0xE00, RegionStatus::Done);

        let text = render(&map, 0);
        let lines: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(
            lines,
            vec![
                // current_pos = first non-done region; still copying.
                "0x00001000     ?               1",
                "0x00000000  0x00001000  +",
                "0x00001000  0x00000200  -",
                "0x00001200  0x00000E00  +",
                "0x00002000  0x00002000  ?",
            ]
        );
    }

    #[test]
    fn positions_are_shifted_by_skip() {
        let mut map = RegionMap::from_total(0x1000);
        map.mark(0, 0x1000, RegionStatus::Done);

        let text = render(&map, 0x10000);
        let lines: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(
            lines,
            vec![
                // Everything done: finished status, current_pos at the end.
                "0x00011000     +               1",
                "0x00010000  0x00001000  +",
            ]
        );
    }
}
