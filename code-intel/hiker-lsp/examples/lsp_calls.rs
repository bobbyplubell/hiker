//! Headless proof harness for [`hiker_lsp::LspAdapter`].
//!
//! Usage: `lsp_calls <repo_root> <symbol> [probe]`
//!
//! Spawns rust-analyzer on `<repo_root>` (a real Rust project — first run is SLOW: RA must
//! `cargo metadata` + build proc-macros + index before it answers), resolves `<symbol>` through the
//! `DerivedNodeSource` port, prints its located file:line + a content snippet, then prints the
//! incoming/outgoing call neighbors — each resolved via `locate`. `[probe]` is the readiness probe
//! query (defaults to `<symbol>`); RA is considered ready once `workspace/symbol(probe)` is
//! non-empty.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use hiker_lsp::LspAdapter;
use spec_engine::{DerivedNodeSource, EdgeKind, SourceId};

fn ra_program() -> PathBuf {
    // Prefer the known install, fall back to PATH.
    let local = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".local/bin/rust-analyzer");
    if local.exists() {
        local
    } else {
        PathBuf::from("rust-analyzer")
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: {} <repo_root> <symbol> [probe]", args[0]);
        return ExitCode::FAILURE;
    }
    let repo_root = Path::new(&args[1]);
    let symbol = &args[2];
    let probe = args.get(3).map(String::as_str).unwrap_or(symbol);

    let source = SourceId("lsp".to_string());
    eprintln!("spawning rust-analyzer on {} (probe {probe:?})...", repo_root.display());
    let adapter = match LspAdapter::spawn(&ra_program(), repo_root, probe, source.clone()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("rust-analyzer failed to become ready: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("rust-analyzer ready.");

    let Some(handle) = adapter.resolve(symbol, &source) else {
        eprintln!("no symbol resolved for {symbol:?}");
        return ExitCode::FAILURE;
    };
    print_located("resolved", &adapter, &handle);
    if let Some(snippet) = adapter.content(&handle) {
        let preview: Vec<&str> = snippet.lines().take(4).collect();
        println!("  snippet:");
        for line in preview {
            println!("    {line}");
        }
    }

    let neighbors = adapter.neighbors(&handle, &[EdgeKind::Calls]);
    println!("call neighbors ({}):", neighbors.len());
    for n in &neighbors {
        print_located("  -", &adapter, n);
    }
    ExitCode::SUCCESS
}

fn print_located(label: &str, adapter: &LspAdapter, handle: &spec_engine::NodeHandle) {
    match adapter.locate(handle) {
        Some(loc) => println!("{label}: {}:{}-{}", loc.file, loc.start_line + 1, loc.end_line + 1),
        None => println!("{label}: <unresolved> {}", handle.id),
    }
}
