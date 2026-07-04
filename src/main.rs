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

    if cli.status {
        return print_status(&cli);
    }

    match exhume::run(&cli) {
        Ok(summary) => {
            if cli.json || cli.json_progress {
                // Compact in event-stream mode, so the summary is one NDJSON
                // line like the progress events before it.
                print_json(&summary, cli.json_progress);
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

/// `--status` mode: load, render, done — nothing is copied.
fn print_status(cli: &Cli) -> ExitCode {
    match exhume::status::report(cli) {
        Ok(report) => {
            if cli.json {
                match serde_json::to_string_pretty(&report) {
                    Ok(json) => println!("{json}"),
                    Err(err) => eprintln!("exhume: failed to render JSON status: {err}"),
                }
            } else {
                print!("{}", exhume::status::render(&report));
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("exhume: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Render the run outcome to stdout (the program's intentional output).
fn print_summary(s: &Summary) {
    print_copy_summary(s);
    if let Some(v) = &s.verify {
        print_verify_summary(v);
    }
}

/// The copy half of the summary.
fn print_copy_summary(s: &Summary) {
    if s.interrupted {
        println!(
            "Interrupted — {} copied so far. Resume with the same command (state: {}).",
            HumanBytes(s.bytes_done),
            s.state_path.display()
        );
    } else if s.completed {
        if s.refreshed {
            let savings = if s.bytes_skipped_by_hash > 0 {
                format!(
                    "{} unchanged per manifest",
                    HumanBytes(s.bytes_skipped_by_hash)
                )
            } else {
                "target comparison".to_string()
            };
            println!(
                "Refreshed — scanned {} from {}, wrote {} to {} ({}).",
                HumanBytes(s.bytes_done_this_run),
                s.source.display(),
                HumanBytes(s.bytes_written_this_run),
                s.target.display(),
                savings
            );
        } else if s.bytes_done_this_run == 0 && s.bytes_done > 0 {
            // A no-op resume: the state was already complete.
            println!(
                "Already complete — {} in {}; nothing copied this run.",
                HumanBytes(s.bytes_done),
                s.target.display()
            );
        } else if s.skip_unchanged || s.skip_zeros {
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

/// The verify half of the summary, when a `--verify` pass ran.
fn print_verify_summary(v: &exhume::engine::VerifyOutcome) {
    let unhashed = if v.chunks_unhashed > 0 {
        format!(
            " ({} chunk(s) have no digest and were skipped)",
            v.chunks_unhashed
        )
    } else {
        String::new()
    };
    if v.interrupted {
        println!("Verify interrupted — re-run with --verify to check again.");
    } else if v.mismatches.is_empty() {
        println!(
            "Verified — {} in {} chunk(s) match the manifest.{}",
            HumanBytes(v.bytes_verified),
            v.chunks_checked,
            unhashed
        );
    } else {
        println!(
            "Verify FAILED — {} of {} chunk(s) differ from the manifest \
             (first at offset {}).{}",
            v.mismatches.len(),
            v.chunks_checked,
            v.mismatches[0],
            unhashed
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
    bytes_done_this_run: u64,
    bytes_written: u64,
    bytes_written_this_run: u64,
    /// Bytes skipped without target I/O on a `--refresh` (manifest match).
    bytes_skipped_by_hash: u64,
    refreshed: bool,
    bad_bytes: u64,
    bad_regions: usize,
    skip_unchanged: bool,
    skip_zeros: bool,
    completed: bool,
    interrupted: bool,
    /// Present when a `--verify` pass ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    verify: Option<JsonVerify>,
}

/// Machine-readable view of a verify pass.
#[derive(serde::Serialize)]
struct JsonVerify {
    ok: bool,
    chunks_checked: u64,
    chunks_unhashed: u64,
    bytes_verified: u64,
    /// Domain offsets of mismatching chunks.
    mismatches: Vec<u64>,
    interrupted: bool,
}

/// Render the run outcome as a single JSON object on stdout.
fn print_json(s: &Summary, compact: bool) {
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
        bytes_done_this_run: s.bytes_done_this_run,
        bytes_written: s.bytes_written,
        bytes_written_this_run: s.bytes_written_this_run,
        bytes_skipped_by_hash: s.bytes_skipped_by_hash,
        refreshed: s.refreshed,
        bad_bytes: s.bad_bytes,
        bad_regions: s.bad_regions,
        skip_unchanged: s.skip_unchanged,
        skip_zeros: s.skip_zeros,
        completed: s.completed,
        interrupted: s.interrupted,
        verify: s.verify.as_ref().map(|v| JsonVerify {
            ok: v.ok(),
            chunks_checked: v.chunks_checked,
            chunks_unhashed: v.chunks_unhashed,
            bytes_verified: v.bytes_verified,
            mismatches: v.mismatches.clone(),
            interrupted: v.interrupted,
        }),
    };
    // Serialisation of this fixed struct cannot fail; fall back rather than panic.
    let rendered = if compact {
        serde_json::to_string(&report)
    } else {
        serde_json::to_string_pretty(&report)
    };
    match rendered {
        Ok(json) => println!("{json}"),
        Err(err) => eprintln!("exhume: failed to render JSON summary: {err}"),
    }
}

/// 0 = fully done, 130 = interrupted (copy or verify), 2 = finished with bad
/// regions, 3 = the verify pass found mismatches.
fn exit_code(s: &Summary) -> ExitCode {
    let verify = s.verify.as_ref();
    if s.interrupted || verify.is_some_and(|v| v.interrupted) {
        ExitCode::from(130)
    } else if !s.completed {
        ExitCode::from(2)
    } else if verify.is_some_and(|v| !v.mismatches.is_empty()) {
        ExitCode::from(3)
    } else {
        ExitCode::SUCCESS
    }
}
