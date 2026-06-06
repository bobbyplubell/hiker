//! The `hiker` CLI. Thin adapters over `hiker-core`: parse args, call the
//! library, print a result. Web-source acquisition verbs (scrape / refresh /
//! crawl / feed) have moved to external producers; see docs/import.md.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("hiker: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// Dispatch one CLI invocation. Returns a human-readable error string on
/// failure (printed to stderr by `main`).
fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("help" | "--help" | "-h") | None => {
            print_usage();
            Ok(())
        }
        Some(other) => Err(format!("unknown command `{other}` (try `hiker help`)")),
    }
}

fn print_usage() {
    println!(
        "hiker — vault CLI\n\n\
         USAGE:\n\
         \x20 hiker help"
    );
}
