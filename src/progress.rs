//! Progress reporting, kept separate from the copy logic so the engine never
//! does its own printing. Draws to stderr (indicatif's default) so the final
//! summary on stdout stays clean and pipeable.

use indicatif::{ProgressBar, ProgressStyle};

/// A thin wrapper around an indicatif progress bar. When the total size is
/// known it shows a real bar with ETA; when unknown it falls back to a spinner;
/// when `quiet` it is hidden entirely.
pub struct Reporter {
    bar: ProgressBar,
}

impl Reporter {
    /// Create a reporter for a domain of `total` bytes (`0` = unknown),
    /// starting at `start` bytes already processed (for resume).
    #[must_use]
    pub fn new(total: u64, start: u64, quiet: bool) -> Self {
        let bar = if quiet {
            ProgressBar::hidden()
        } else if total > 0 {
            let bar = ProgressBar::new(total);
            bar.set_style(
                ProgressStyle::with_template(
                    "{bytes}/{total_bytes} ({percent}%) {bytes_per_sec} ETA {eta} [{wide_bar}]",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("=>-"),
            );
            bar
        } else {
            let bar = ProgressBar::new_spinner();
            bar.set_style(
                ProgressStyle::with_template("{spinner} {bytes} copied ({bytes_per_sec})")
                    .unwrap_or_else(|_| ProgressStyle::default_spinner()),
            );
            bar
        };
        bar.set_position(start);
        Self { bar }
    }

    /// Advance the bar by `n` processed bytes.
    pub fn inc(&self, n: u64) {
        self.bar.inc(n);
    }

    /// Remove the bar from the terminal once the copy is finished.
    pub fn finish(&self) {
        self.bar.finish_and_clear();
    }
}
