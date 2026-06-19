//! Headless smoke harness for the [`zim_adapter::ZimAdapter`].
//!
//! Usage: `zim_neighbors <archive.zim> <title-query>`
//!
//! Opens the archive, resolves the title query to an article through the `DerivedNodeSource` port,
//! and prints: the located entry path, a tag-stripped snippet of the article content, the neighbor
//! article ids (in-archive hyperlinks via `EdgeKind::Link`), and the article's drift fingerprint.
//! Used for manual smoke tests against a real ZIM; the crate's synthetic tests are the automated
//! proof.

use std::path::Path;
use std::process::ExitCode;

use spec_engine::{DerivedNodeSource, EdgeKind, SourceId};
use zim_adapter::ZimAdapter;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: {} <archive.zim> <title-query>", args[0]);
        return ExitCode::FAILURE;
    }
    let archive_path = Path::new(&args[1]);
    let query = &args[2];

    let source = SourceId("zim".to_string());
    let adapter = match ZimAdapter::open(archive_path, source.clone()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("failed to open {}: {e}", archive_path.display());
            return ExitCode::FAILURE;
        }
    };

    let Some(handle) = adapter.resolve(query, &source) else {
        eprintln!("no article resolved for query {query:?}");
        return ExitCode::FAILURE;
    };
    println!("resolved: {}", handle.id);

    if let Some(loc) = adapter.locate(&handle) {
        println!("locate:   {} ({}..{})", loc.file, loc.start_line, loc.end_line);
    }

    if let Some(content) = adapter.content(&handle) {
        let snippet: String = strip_tags(&content).chars().take(200).collect();
        println!("content:  {}", snippet.trim());
    }

    let neighbors = adapter.neighbors(&handle, &[EdgeKind::Link]);
    println!("neighbors ({}):", neighbors.len());
    for n in &neighbors {
        println!("  - {}", n.id);
    }

    if let Some(fp) = adapter.fingerprint(&handle) {
        println!("fingerprint: {}", fp.0);
    }

    ExitCode::SUCCESS
}

/// Crude tag stripper for the snippet: drop everything between `<` and `>`, collapse whitespace.
fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}
