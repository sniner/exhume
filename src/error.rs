//! Crate error type. The library returns [`Result`]; the binary renders the
//! [`Error`] at the boundary and maps it to an exit code.

use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    /// An I/O operation failed; `context` describes what we were doing.
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    /// A size argument (sector size, transfer size, length) could not be parsed
    /// or was out of range.
    #[error("invalid size: {0}")]
    InvalidSize(String),

    /// A byte offset (`--skip` / `--seek`) is not a multiple of the sector size.
    /// The carried message names the offset and suggests the nearest aligned
    /// values.
    #[error("{0}")]
    Misaligned(String),

    /// The target already exists and neither `--force` nor a resume applies.
    #[error(
        "target '{}' already exists; pass --force to overwrite (a matching state file enables resume instead)",
        .0.display()
    )]
    TargetExists(PathBuf),

    /// A preflight safety check refused the copy: source and target are the
    /// same file, the target is mounted or too small, or the source type is
    /// unsupported. The carried message names the problem and the way out.
    #[error("{0}")]
    Refused(String),

    /// A command-line parameter conflicts with the resumed state file (the
    /// region map's coordinates depend on target, skip, seek, and the sector
    /// grid, so a silent mismatch would corrupt the copy). The carried message
    /// names the conflict and how to resolve it.
    #[error("{0}")]
    StateConflict(String),

    /// The state file exists but could not be parsed as exhume state.
    #[error("failed to parse state file '{}': {source}", .path.display())]
    StateParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// Serialising the in-memory state to TOML failed.
    #[error("failed to serialise state: {0}")]
    StateSerialise(#[from] toml::ser::Error),

    /// Installing the SIGINT/SIGTERM handler failed.
    #[error("failed to install signal handler: {0}")]
    Signal(String),
}

impl Error {
    /// Helper to wrap an [`std::io::Error`] with human-readable context.
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Error::Io {
            context: context.into(),
            source,
        }
    }
}
