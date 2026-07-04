//! exhume — a modern, resumable disk imaging and rescue tool.
//!
//! exhume reads a source (block device or file) block-wise and writes it to a
//! target, recording its progress in a human-readable TOML state file. An
//! interrupted run resumes by simply re-running the same command, and read
//! errors are recorded and skipped instead of aborting the whole copy.

pub mod cli;
pub mod engine;
pub mod error;
pub mod params;
pub mod progress;
pub mod region;
pub mod safety;
pub mod state;

pub use engine::{Summary, run};
pub use error::{Error, Result};
