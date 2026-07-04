//! Progress reporting, kept separate from the copy logic so the engine never
//! does its own printing. The bar draws to stderr (indicatif's default) so the
//! final summary on stdout stays clean and pipeable; the optional
//! `--json-progress` event stream goes to stdout as NDJSON, one self-contained
//! object per line, for cron jobs, GUIs, and orchestration.

use std::cell::Cell;
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressStyle};

/// Minimum spacing between two progress events.
const EVENT_INTERVAL: Duration = Duration::from_secs(2);

/// A thin wrapper around an indicatif progress bar. When the total size is
/// known it shows a real bar with ETA; when unknown it falls back to a spinner;
/// when `quiet` it is hidden entirely. With an event phase set, it also emits
/// NDJSON progress events on stdout.
pub struct Reporter {
    bar: ProgressBar,
    events: Option<EventEmitter>,
}

impl Reporter {
    /// Create a reporter for a domain of `total` bytes (`0` = unknown),
    /// starting at `start` bytes already processed (for resume). `events`
    /// names the phase (`"copy"`, `"verify"`) to emit NDJSON progress events
    /// for, or `None` for the bar only.
    #[must_use]
    pub fn new(total: u64, start: u64, quiet: bool, events: Option<&'static str>) -> Self {
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
        let events = events.map(|phase| EventEmitter::new(phase, total, start));
        Self { bar, events }
    }

    /// Advance the bar by `n` processed bytes.
    pub fn inc(&self, n: u64) {
        self.bar.inc(n);
        if let Some(events) = &self.events {
            events.tick(n);
        }
    }

    /// Remove the bar from the terminal once the pass is finished.
    pub fn finish(&self) {
        self.bar.finish_and_clear();
    }
}

/// Emits one NDJSON line per [`EVENT_INTERVAL`], plus one immediately on
/// construction so even short runs produce a signal.
struct EventEmitter {
    phase: &'static str,
    total: u64,
    baseline: u64,
    start: Instant,
    done: Cell<u64>,
    last: Cell<Instant>,
}

impl EventEmitter {
    fn new(phase: &'static str, total: u64, baseline: u64) -> Self {
        let emitter = EventEmitter {
            phase,
            total,
            baseline,
            start: Instant::now(),
            done: Cell::new(baseline),
            last: Cell::new(Instant::now()),
        };
        emitter.emit();
        emitter
    }

    fn tick(&self, n: u64) {
        self.done.set(self.done.get() + n);
        if self.last.get().elapsed() >= EVENT_INTERVAL {
            self.emit();
            self.last.set(Instant::now());
        }
    }

    /// One self-contained progress line. The rate covers this run only (the
    /// resumed baseline is excluded).
    fn emit(&self) {
        let elapsed = self.start.elapsed().as_secs_f64();
        let done = self.done.get();
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "throughput display value; precision is irrelevant"
        )]
        let rate = if elapsed > 0.0 {
            ((done - self.baseline) as f64 / elapsed) as u64
        } else {
            0
        };
        let event = serde_json::json!({
            "status": "running",
            "phase": self.phase,
            "bytes_total": self.total,
            "bytes_done": done,
            "elapsed_seconds": (elapsed * 10.0).round() / 10.0,
            "bytes_per_second": rate,
        });
        println!("{event}");
    }
}
