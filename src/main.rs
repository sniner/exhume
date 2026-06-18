//! exhume binary entry point: parse arguments, set up logging, run the copy,
//! and render the outcome.

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
            print_summary(&summary);
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
