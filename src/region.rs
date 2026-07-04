//! The region map: a sorted, non-overlapping list of extents that records the
//! status of every byte in the copy domain. A fully-copied disk collapses to a
//! single `Done` region. This is the data structure that makes resume and
//! error-aware retry possible — the spiritual cousin of ddrescue's mapfile.

use serde::{Deserialize, Serialize};

/// Status of a contiguous run of bytes in the copy domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RegionStatus {
    /// Not yet attempted.
    Untried,
    /// Successfully read and written.
    Done,
    /// A read error occurred here.
    Bad,
}

/// A contiguous extent in domain coordinates (offsets relative to `skip`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    pub start: u64,
    pub length: u64,
    pub status: RegionStatus,
}

impl Region {
    /// One past the last byte of this region.
    #[must_use]
    pub fn end(&self) -> u64 {
        self.start + self.length
    }
}

/// A normalised collection of regions: sorted by `start`, non-overlapping, with
/// adjacent same-status regions merged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegionMap {
    regions: Vec<Region>,
}

impl RegionMap {
    /// A map covering `[0, total)` as a single `Untried` region (empty if
    /// `total == 0`, e.g. for a source of unknown size).
    #[must_use]
    pub fn from_total(total: u64) -> Self {
        let mut map = RegionMap::default();
        if total > 0 {
            map.regions.push(Region {
                start: 0,
                length: total,
                status: RegionStatus::Untried,
            });
        }
        map
    }

    /// Build a normalised map from a (possibly unsorted) list of regions, as
    /// loaded from a state file. Assumes the input does not overlap.
    #[must_use]
    pub fn from_regions(mut regions: Vec<Region>) -> Self {
        regions.retain(|r| r.length > 0);
        regions.sort_by_key(|r| r.start);
        RegionMap {
            regions: merge_adjacent(regions),
        }
    }

    /// Set the status of `[start, start + length)`, splitting any regions that
    /// straddle the boundaries and merging neighbours of equal status.
    pub fn mark(&mut self, start: u64, length: u64, status: RegionStatus) {
        if length == 0 {
            return;
        }
        let end = start + length;
        let mut next: Vec<Region> = Vec::with_capacity(self.regions.len() + 2);
        for r in &self.regions {
            if r.end() <= start || r.start >= end {
                // No overlap — keep as is.
                next.push(*r);
                continue;
            }
            // Preserve the parts of `r` outside the marked range.
            if r.start < start {
                next.push(Region {
                    start: r.start,
                    length: start - r.start,
                    status: r.status,
                });
            }
            if r.end() > end {
                next.push(Region {
                    start: end,
                    length: r.end() - end,
                    status: r.status,
                });
            }
        }
        next.push(Region {
            start,
            length,
            status,
        });
        next.sort_by_key(|r| r.start);
        self.regions = merge_adjacent(next);
    }

    /// The regions currently in `status`, in order.
    #[must_use]
    pub fn regions_with(&self, status: RegionStatus) -> Vec<Region> {
        self.regions
            .iter()
            .copied()
            .filter(|r| r.status == status)
            .collect()
    }

    /// Total number of bytes currently in `status`.
    #[must_use]
    pub fn bytes_with(&self, status: RegionStatus) -> u64 {
        self.regions
            .iter()
            .filter(|r| r.status == status)
            .map(|r| r.length)
            .sum()
    }

    /// Reconcile the map with a copy domain of `domain` bytes: clip regions
    /// that reach beyond it and fill uncovered gaps (interior or tail) as
    /// `Untried`. Needed when a resumed state file was recorded against a
    /// different domain — a larger `--length`, or a source that changed size —
    /// so the new tail is actually copied and an out-of-domain remainder is
    /// not. A `domain` of `0` means "unknown size" and leaves the map alone
    /// (the sequential path resumes from [`Self::covered_end`]).
    pub fn reconcile(&mut self, domain: u64) {
        if domain == 0 {
            return;
        }
        let mut next: Vec<Region> = Vec::with_capacity(self.regions.len() + 2);
        let mut pos = 0u64;
        for r in &self.regions {
            if r.start >= domain {
                break;
            }
            if r.start > pos {
                next.push(Region {
                    start: pos,
                    length: r.start - pos,
                    status: RegionStatus::Untried,
                });
            }
            let length = r.length.min(domain - r.start);
            next.push(Region {
                start: r.start,
                length,
                status: r.status,
            });
            pos = r.start + length;
        }
        if pos < domain {
            next.push(Region {
                start: pos,
                length: domain - pos,
                status: RegionStatus::Untried,
            });
        }
        self.regions = merge_adjacent(next);
    }

    /// One past the last covered byte (regions are contiguous and sorted, so
    /// this is the highest `end()`), or `0` when empty.
    #[must_use]
    pub fn covered_end(&self) -> u64 {
        self.regions.last().map_or(0, Region::end)
    }

    /// Whether every byte of `[start, end)` currently has `status`. An empty
    /// range is trivially covered.
    #[must_use]
    pub fn covers(&self, start: u64, end: u64, status: RegionStatus) -> bool {
        let mut pos = start;
        for r in &self.regions {
            if pos >= end {
                break;
            }
            if r.end() <= pos {
                continue;
            }
            if r.start > pos || r.status != status {
                return false;
            }
            pos = r.end();
        }
        pos >= end
    }

    /// Read-only view of the underlying regions.
    #[must_use]
    pub fn regions(&self) -> &[Region] {
        &self.regions
    }
}

/// Merge adjacent regions of equal status. Input must already be sorted by
/// `start` and non-overlapping.
fn merge_adjacent(regions: Vec<Region>) -> Vec<Region> {
    let mut merged: Vec<Region> = Vec::with_capacity(regions.len());
    for r in regions {
        if let Some(last) = merged.last_mut() {
            if last.status == r.status && last.end() == r.start {
                last.length += r.length;
                continue;
            }
        }
        merged.push(r);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::{Region, RegionMap, RegionStatus};

    fn statuses(map: &RegionMap) -> Vec<(u64, u64, RegionStatus)> {
        map.regions()
            .iter()
            .map(|r| (r.start, r.length, r.status))
            .collect()
    }

    #[test]
    fn fresh_map_is_one_untried_region() {
        let map = RegionMap::from_total(1000);
        assert_eq!(statuses(&map), vec![(0, 1000, RegionStatus::Untried)]);
    }

    #[test]
    fn empty_total_is_empty_map() {
        let map = RegionMap::from_total(0);
        assert!(map.regions().is_empty());
        assert_eq!(map.covered_end(), 0);
    }

    #[test]
    fn marking_the_front_splits_and_merges() {
        let mut map = RegionMap::from_total(1000);
        map.mark(0, 100, RegionStatus::Done);
        map.mark(100, 100, RegionStatus::Done);
        // Two adjacent Done marks collapse into one region.
        assert_eq!(
            statuses(&map),
            vec![
                (0, 200, RegionStatus::Done),
                (200, 800, RegionStatus::Untried),
            ]
        );
    }

    #[test]
    fn marking_the_middle_bad_splits_into_three() {
        let mut map = RegionMap::from_total(1000);
        map.mark(400, 100, RegionStatus::Bad);
        assert_eq!(
            statuses(&map),
            vec![
                (0, 400, RegionStatus::Untried),
                (400, 100, RegionStatus::Bad),
                (500, 500, RegionStatus::Untried),
            ]
        );
        assert_eq!(map.bytes_with(RegionStatus::Bad), 100);
        assert_eq!(map.bytes_with(RegionStatus::Untried), 900);
    }

    #[test]
    fn remarking_overrides_previous_status() {
        let mut map = RegionMap::from_total(1000);
        map.mark(400, 100, RegionStatus::Bad);
        // A later successful retry of the same block flips it to Done.
        map.mark(400, 100, RegionStatus::Done);
        assert_eq!(map.bytes_with(RegionStatus::Bad), 0);
        assert_eq!(map.bytes_with(RegionStatus::Done), 100);
    }

    #[test]
    fn overlapping_mark_clips_neighbours() {
        let mut map = RegionMap::from_total(1000);
        map.mark(100, 200, RegionStatus::Done); // [100,300) Done
        map.mark(250, 100, RegionStatus::Bad); // overlaps the tail of Done
        assert_eq!(
            statuses(&map),
            vec![
                (0, 100, RegionStatus::Untried),
                (100, 150, RegionStatus::Done),
                (250, 100, RegionStatus::Bad),
                (350, 650, RegionStatus::Untried),
            ]
        );
    }

    #[test]
    fn reconcile_fills_a_grown_domain_as_untried() {
        let mut map = RegionMap::from_total(1000);
        map.mark(0, 1000, RegionStatus::Done);
        map.reconcile(1500);
        assert_eq!(
            statuses(&map),
            vec![
                (0, 1000, RegionStatus::Done),
                (1000, 500, RegionStatus::Untried),
            ]
        );
    }

    #[test]
    fn reconcile_clips_a_shrunk_domain() {
        let mut map = RegionMap::from_total(1000);
        map.mark(900, 100, RegionStatus::Bad);
        map.reconcile(950);
        assert_eq!(
            statuses(&map),
            vec![
                (0, 900, RegionStatus::Untried),
                (900, 50, RegionStatus::Bad),
            ]
        );
    }

    #[test]
    fn reconcile_fills_interior_gaps() {
        // A hand-edited state file may leave holes between regions.
        let mut map = RegionMap::from_regions(vec![Region {
            start: 200,
            length: 100,
            status: RegionStatus::Done,
        }]);
        map.reconcile(400);
        assert_eq!(
            statuses(&map),
            vec![
                (0, 200, RegionStatus::Untried),
                (200, 100, RegionStatus::Done),
                (300, 100, RegionStatus::Untried),
            ]
        );
    }

    #[test]
    fn reconcile_leaves_a_matching_map_and_unknown_domain_alone() {
        let mut map = RegionMap::from_total(1000);
        map.mark(0, 500, RegionStatus::Done);
        let before = map.clone();
        map.reconcile(1000);
        assert_eq!(map, before);
        map.reconcile(0); // unknown size: untouched
        assert_eq!(map, before);
    }

    #[test]
    fn covers_checks_status_over_a_range() {
        let mut map = RegionMap::from_total(1000);
        map.mark(0, 500, RegionStatus::Done);
        map.mark(200, 100, RegionStatus::Bad);

        assert!(map.covers(0, 200, RegionStatus::Done));
        assert!(map.covers(300, 500, RegionStatus::Done));
        assert!(!map.covers(0, 500, RegionStatus::Done), "bad hole inside");
        assert!(!map.covers(400, 600, RegionStatus::Done), "untried tail");
        assert!(map.covers(200, 300, RegionStatus::Bad));
        assert!(map.covers(100, 100, RegionStatus::Bad), "empty range");
        assert!(
            !map.covers(900, 1100, RegionStatus::Untried),
            "past the end"
        );
    }

    #[test]
    fn from_regions_normalises() {
        let map = RegionMap::from_regions(vec![
            Region {
                start: 100,
                length: 100,
                status: RegionStatus::Done,
            },
            Region {
                start: 0,
                length: 100,
                status: RegionStatus::Done,
            },
        ]);
        assert_eq!(statuses(&map), vec![(0, 200, RegionStatus::Done)]);
    }
}
