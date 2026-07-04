//! The TOML state file.
//!
//! It is intended to be read, understood, and even hand-edited by a human, and
//! is sufficient to repeat a run without re-specifying parameters. Layout:
//!
//! ```toml
//! [meta]            # format version + provenance
//! [params]          # the full run configuration (see RunParams)
//! [progress]        # human-facing totals (derivable from regions)
//! [hashes]          # optional integrity manifest: per-chunk digests
//! [[regions]]       # the region map: start / length / status
//! ```

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::params::RunParams;
use crate::region::{Region, RegionMap, RegionStatus};

/// Provenance and format version of a state file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub version: u32,
    pub program: String,
    pub program_version: String,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
}

/// Human-facing progress totals. These are derivable from the region map but
/// are written out so the file is informative at a glance.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Progress {
    /// Size of the copy domain in bytes, or `0` if unknown.
    #[serde(default)]
    pub bytes_total: u64,
    #[serde(default)]
    pub bytes_done: u64,
    /// Bytes actually written to the target (< `bytes_done` in skip-unchanged
    /// mode, where unchanged blocks are not rewritten).
    #[serde(default)]
    pub bytes_written: u64,
    /// Number of bad regions recorded.
    #[serde(default)]
    pub errors: u64,
}

/// The integrity manifest: one digest per fixed-size chunk of the domain.
/// Chunks are indexed implicitly by position in `chunks`; an empty string
/// means "not hashed" (the chunk was never streamed in one piece, e.g. it
/// contains bad sectors).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hashes {
    /// Digest algorithm; currently always `"blake3"`.
    pub algorithm: String,
    /// Chunk size in bytes; the grid is fixed for the lifetime of the state.
    pub chunk_size: u64,
    /// Lowercase hex digests by chunk index; `""` = not hashed.
    #[serde(default)]
    pub chunks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateFile {
    pub meta: Meta,
    pub params: RunParams,
    #[serde(default)]
    pub progress: Progress,
    /// Optional integrity manifest (`[hashes]`), present when hashing was on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hashes: Option<Hashes>,
    #[serde(default)]
    pub regions: Vec<Region>,
}

impl StateFile {
    /// Current state-file format version. Starts at 1 with the first release;
    /// the pre-release schema churn (the flat `block_size` / `count` becoming
    /// `sector_size` / `transfer_size` / `length`) needs no bump, as no v1 file
    /// with the old layout was ever shipped.
    pub const VERSION: u32 = 1;

    /// Build a state file from the live copy state.
    #[must_use]
    pub fn build(
        params: &RunParams,
        map: &RegionMap,
        domain: u64,
        created: DateTime<Utc>,
        bytes_written: u64,
        hashes: Option<Hashes>,
    ) -> Self {
        StateFile {
            meta: Meta {
                version: Self::VERSION,
                program: env!("CARGO_PKG_NAME").to_string(),
                program_version: env!("CARGO_PKG_VERSION").to_string(),
                created,
                updated: Utc::now(),
            },
            params: params.clone(),
            hashes,
            progress: Progress {
                bytes_total: domain,
                bytes_done: map.bytes_with(RegionStatus::Done),
                bytes_written,
                errors: map
                    .regions()
                    .iter()
                    .filter(|r| r.status == RegionStatus::Bad)
                    .count() as u64,
            },
            regions: map.regions().to_vec(),
        }
    }

    /// The region map described by this state file.
    #[must_use]
    pub fn region_map(&self) -> RegionMap {
        RegionMap::from_regions(self.regions.clone())
    }

    /// Load and parse a state file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or is not valid exhume state.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::io(format!("reading state file '{}'", path.display()), e))?;
        let state: Self = toml::from_str(&text).map_err(|source| Error::StateParse {
            path: path.to_path_buf(),
            source,
        })?;
        if state.meta.version > Self::VERSION {
            tracing::warn!(
                found = state.meta.version,
                supported = Self::VERSION,
                "state file was written by a newer exhume; proceeding may misbehave"
            );
        }
        Ok(state)
    }

    /// Load the state file if it exists, otherwise `None`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn load_if_exists(path: &Path) -> Result<Option<Self>> {
        if path.exists() {
            Ok(Some(Self::load(path)?))
        } else {
            Ok(None)
        }
    }

    /// Write the state file atomically: serialise to a sibling `*.tmp` file,
    /// sync it, and rename over the target, so neither a crash mid-write nor a
    /// power loss right after the rename can leave a corrupt or empty state
    /// (the rename may otherwise reach the disk before the data blocks do).
    ///
    /// # Errors
    ///
    /// Returns an error if serialisation, the temp write, the sync, or the
    /// rename fails.
    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        use std::io::Write;

        let text = toml::to_string_pretty(self)?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let mut file = std::fs::File::create(&tmp)
            .map_err(|e| Error::io(format!("creating state file '{}'", tmp.display()), e))?;
        file.write_all(text.as_bytes())
            .map_err(|e| Error::io(format!("writing state file '{}'", tmp.display()), e))?;
        file.sync_all()
            .map_err(|e| Error::io(format!("syncing state file '{}'", tmp.display()), e))?;
        drop(file);
        std::fs::rename(&tmp, path)
            .map_err(|e| Error::io(format!("renaming state file to '{}'", path.display()), e))?;
        // Best-effort: persist the rename itself (the directory entry). A
        // failure only means the old state survives a power loss a little
        // longer — not worth failing the copy over.
        let dir = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        if let Ok(dir) = std::fs::File::open(dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::StateFile;
    use crate::params::RunParams;
    use crate::region::{RegionMap, RegionStatus};
    use chrono::Utc;
    use std::path::PathBuf;

    fn sample_params() -> RunParams {
        RunParams {
            source: PathBuf::from("/dev/sdb"),
            target: PathBuf::from("grave.img"),
            sector_size: 512,
            transfer_size: 1 << 20,
            skip: 0,
            seek: 0,
            length: 0,
            skip_unchanged: false,
            skip_zeros: false,
        }
    }

    #[test]
    fn round_trips_through_toml() {
        let mut map = RegionMap::from_total(4096);
        map.mark(0, 2048, RegionStatus::Done);
        map.mark(2048, 1024, RegionStatus::Bad);
        let state = StateFile::build(&sample_params(), &map, 4096, Utc::now(), 2048, None);

        let text = toml::to_string_pretty(&state).unwrap();
        assert!(text.contains("[meta]"));
        assert!(text.contains("[params]"));
        assert!(text.contains("[[regions]]"));
        assert!(!text.contains("[hashes]"), "no manifest unless hashing ran");

        let parsed: StateFile = toml::from_str(&text).unwrap();
        assert_eq!(parsed.params, sample_params());
        assert_eq!(parsed.progress.bytes_done, 2048);
        assert_eq!(parsed.progress.bytes_written, 2048);
        assert_eq!(parsed.progress.errors, 1);
        assert_eq!(parsed.hashes, None);
        assert_eq!(parsed.region_map(), map);
    }

    #[test]
    fn hashes_round_trip_through_toml() {
        use super::Hashes;

        let mut map = RegionMap::from_total(4096);
        map.mark(0, 4096, RegionStatus::Done);
        let hashes = Hashes {
            algorithm: "blake3".to_string(),
            chunk_size: 2048,
            chunks: vec!["aa11".to_string(), String::new()],
        };
        let state = StateFile::build(
            &sample_params(),
            &map,
            4096,
            Utc::now(),
            4096,
            Some(hashes.clone()),
        );

        let text = toml::to_string_pretty(&state).unwrap();
        assert!(text.contains("[hashes]"), "state was:\n{text}");

        let parsed: StateFile = toml::from_str(&text).unwrap();
        assert_eq!(parsed.hashes, Some(hashes));
    }
}
