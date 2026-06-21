//! exhume binary entry point: parse arguments, set up logging, run the copy,
//! and render the outcome.

use std::path::Path;
use std::process::ExitCode;

use clap::Parser;
use exhume::Summary;
use exhume::cli::Cli;
use indicatif::HumanBytes;

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match exhume::run(&cli) {
        Ok(summary) => {
            if cli.json {
                print_json(&summary);
            } else {
                print_summary(&summary);
            }
            exit_code(&summary)
        }
        Err(err) => {
            tracing::error!("{err}");
            eprintln!("exhume: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Initialise the tracing subscriber. Verbosity escalates with `-v`; `RUST_LOG`
/// overrides it entirely. Diagnostics go to stderr.
fn init_tracing(verbose: u8) {
    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(format!("exhume={level}")));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .init();
}

/// Render the run outcome to stdout (the program's intentional output).
fn print_summary(s: &Summary) {
    if s.interrupted {
        println!(
            "Interrupted — {} copied so far. Resume with the same command (state: {}).",
            HumanBytes(s.bytes_done),
            s.state_path.display()
        );
    } else if s.completed {
        if s.skip_unchanged || s.skip_zeros {
            println!(
                "Done — scanned {} from {}, wrote {} to {} ({}).",
                HumanBytes(s.bytes_done),
                s.source.display(),
                HumanBytes(s.bytes_written),
                s.target.display(),
                skip_reason(s)
            );
        } else {
            println!(
                "Done — copied {} from {} to {}.",
                HumanBytes(s.bytes_done),
                s.source.display(),
                s.target.display()
            );
        }
    } else {
        println!(
            "Finished with errors — {} copied, {} unreadable in {} bad region(s). \
             Re-run to retry (state: {}).",
            HumanBytes(s.bytes_done),
            HumanBytes(s.bad_bytes),
            s.bad_regions,
            s.state_path.display()
        );
    }
}

/// Short phrase describing why fewer bytes were written than scanned.
fn skip_reason(s: &Summary) -> &'static str {
    match (s.skip_unchanged, s.skip_zeros) {
        (true, true) => "unchanged and zero blocks skipped",
        (true, false) => "rest already matched",
        (false, true) => "zero blocks left sparse",
        (false, false) => "",
    }
}

/// Machine-readable view of a [`Summary`], serialised to stdout under `--json`.
/// Kept separate from the engine's `Summary` so the wire format stays stable
/// independent of the internal struct.
#[derive(serde::Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "wire-format DTO mirroring Summary's independent outcome flags"
)]
struct JsonReport<'a> {
    /// `"completed"`, `"interrupted"`, or `"errors"` — mirrors the exit code.
    status: &'a str,
    source: &'a Path,
    target: &'a Path,
    state: &'a Path,
    bytes_total: u64,
    bytes_done: u64,
    bytes_written: u64,
    bad_bytes: u64,
    bad_regions: usize,
    skip_unchanged: bool,
    skip_zeros: bool,
    completed: bool,
    interrupted: bool,
}

/// Render the run outcome as a single JSON object on stdout.
fn print_json(s: &Summary) {
    let status = if s.completed {
        "completed"
    } else if s.interrupted {
        "interrupted"
    } else {
        "errors"
    };
    let report = JsonReport {
        status,
        source: &s.source,
        target: &s.target,
        state: &s.state_path,
        bytes_total: s.bytes_total,
        bytes_done: s.bytes_done,
        bytes_written: s.bytes_written,
        bad_bytes: s.bad_bytes,
        bad_regions: s.bad_regions,
        skip_unchanged: s.skip_unchanged,
        skip_zeros: s.skip_zeros,
        completed: s.completed,
        interrupted: s.interrupted,
    };
    // Serialisation of this fixed struct cannot fail; fall back rather than panic.
    match serde_json::to_string_pretty(&report) {
        Ok(json) => println!("{json}"),
        Err(err) => eprintln!("exhume: failed to render JSON summary: {err}"),
    }
}

/// 0 = fully done, 130 = interrupted, 2 = finished with bad regions.
fn exit_code(s: &Summary) -> ExitCode {
    if s.completed {
        ExitCode::SUCCESS
    } else if s.interrupted {
        ExitCode::from(130)
    } else {
        ExitCode::from(2)
    }
}
