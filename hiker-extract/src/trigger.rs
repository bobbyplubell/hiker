//! The ingest-trigger decision: how a non-markdown source reaches (or doesn't
//! reach) the registry. Hybrid model — auto for opted-in folders, on-demand
//! everywhere else, ignored by default:
//!
//! - **Auto per glob.** A non-md source matching an `[extract].auto_globs`
//!   entry enqueues an extract job on appear/change (startup scan + watcher).
//!   Default-empty `auto_globs` means nothing auto-extracts
//!   (`extract-trigger-auto-glob`).
//! - **On-demand elsewhere.** A non-md source outside every auto-glob is not
//!   extracted automatically; an explicit "Make searchable" action enqueues a
//!   one-off job (`extract-trigger-on-demand`).
//! - **Ignored by default.** A non-md source neither auto-glob-matched nor
//!   explicitly extracted stays ignored: it keeps the unsupported tree marker
//!   and opens in the OS handler (`extract-trigger-default-ignore`).
//!
//! This module owns the *decision* (pure policy over a vault-relative path +
//! the configured globs). The actual enqueue and the OS-handler open live in
//! the app/cli layer that wires this crate. See `docs/extract.md`
//! "Ingest trigger".
//
// status: extract-trigger-auto-glob
// status: extract-trigger-on-demand
// status: extract-trigger-default-ignore

/// What the trigger model decides for a non-markdown source on
/// appear/change. Markdown sources never reach this — they ride core's
/// ordinary ingest path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Path matches an auto-glob — enqueue an extract job now.
    AutoExtract,
    /// Path is outside every auto-glob — stay ignored (unsupported marker,
    /// OS-handler open) until an explicit "Make searchable" action.
    Ignore,
}

/// Decide the trigger for a vault-relative non-md `rel` path given the
/// configured `auto_globs`. The auto-glob folders auto-extract; everything
/// else is ignored.
///
/// status: extract-trigger-auto-glob
pub fn decide(rel: &str, auto_globs: &[String]) -> Decision {
    if matches_any_glob(rel, auto_globs) {
        Decision::AutoExtract
    } else {
        Decision::Ignore
    }
}

/// Whether `rel` matches any of the gitignore-style `globs` (vault-relative
/// paths). A trailing `/` makes a glob a directory prefix that matches every
/// path under it.
pub fn matches_any_glob(rel: &str, globs: &[String]) -> bool {
    globs.iter().any(|g| glob_matches(g, rel))
}

/// Match a single gitignore-style glob against a vault-relative path.
///
/// Supported syntax (a deliberately small, dependency-free subset — no
/// `globset` crate, honoring the clean-SBOM posture):
/// - a trailing `/` (e.g. `inbox/`) matches the directory and everything
///   under it;
/// - `**` matches any number of path segments (including zero);
/// - `*` matches any run of characters *within* a single path segment (it
///   does not cross `/`);
/// - `?` matches one non-`/` character;
/// - everything else is a literal.
///
/// A glob with no wildcard and no trailing slash is treated as a directory
/// prefix too (so `inbox` matches `inbox/a.pdf`), matching the gitignore
/// folder convention used elsewhere in hiker config.
pub fn glob_matches(glob: &str, path: &str) -> bool {
    let glob = glob.trim_start_matches("./");
    let path = path.trim_start_matches("./");

    // Trailing-slash or wildcard-free entry → directory-prefix match.
    if let Some(dir) = glob.strip_suffix('/') {
        return path == dir || path.starts_with(&format!("{dir}/"));
    }
    if !glob.contains(['*', '?']) {
        return path == glob || path.starts_with(&format!("{glob}/"));
    }

    glob_match_segments(glob, path)
}

/// Backtracking matcher for a glob containing `*` / `**` / `?`. Operates on
/// bytes; `*` stops at `/`, `**` crosses `/`.
fn glob_match_segments(glob: &str, path: &str) -> bool {
    let g: Vec<char> = glob.chars().collect();
    let p: Vec<char> = path.chars().collect();
    matches_from(&g, 0, &p, 0)
}

fn matches_from(g: &[char], mut gi: usize, p: &[char], mut pi: usize) -> bool {
    while gi < g.len() {
        match g[gi] {
            '*' => {
                // `**` (optionally followed by `/`) crosses path separators.
                let double = gi + 1 < g.len() && g[gi + 1] == '*';
                if double {
                    let mut next = gi + 2;
                    if next < g.len() && g[next] == '/' {
                        next += 1;
                    }
                    // Try consuming 0..=remaining path chars.
                    for skip in pi..=p.len() {
                        if matches_from(g, next, p, skip) {
                            return true;
                        }
                    }
                    return false;
                }
                // Single `*`: match within the current segment only.
                for skip in pi..=p.len() {
                    if matches_from(g, gi + 1, p, skip) {
                        return true;
                    }
                    if p.get(skip) == Some(&'/') {
                        break;
                    }
                }
                return false;
            }
            '?' => {
                if pi >= p.len() || p[pi] == '/' {
                    return false;
                }
                gi += 1;
                pi += 1;
            }
            c => {
                if pi >= p.len() || p[pi] != c {
                    return false;
                }
                gi += 1;
                pi += 1;
            }
        }
    }
    pi == p.len()
}
