//! A tiny path-glob matcher for [`crate::Scope`] — enough for `src/**`, `target/**`,
//! `services/a/**`, `**/*.rs`. Avoids pulling a full glob crate for this one bounded need.
//!
//! Semantics (path separator = `/`):
//! - `*`  matches any run of non-`/` characters within a single path segment.
//! - `**` matches any number of whole segments (including zero), i.e. crosses `/`.
//! - everything else is a literal.

/// Match `pattern` against `path` (both `/`-separated, no leading `/`).
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    match_segments(&pat, &path)
}

fn match_segments(pat: &[&str], path: &[&str]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => {
            // `**` consumes zero or more leading path segments.
            if rest.is_empty() {
                return true;
            }
            (0..=path.len()).any(|skip| match_segments(rest, &path[skip..]))
        }
        Some((seg, rest)) => match path.split_first() {
            Some((head, ptail)) if match_segment(seg, head) => match_segments(rest, ptail),
            _ => false,
        },
    }
}

/// Match a single segment pattern (with `*` wildcards) against a single path segment.
fn match_segment(pat: &str, seg: &str) -> bool {
    if !pat.contains('*') {
        return pat == seg;
    }
    // Split on `*`; each literal piece must appear in order, anchored at the ends.
    let parts: Vec<&str> = pat.split('*').collect();
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            // Anchored prefix.
            if !seg[pos..].starts_with(part) {
                return false;
            }
            pos += part.len();
        } else if i == parts.len() - 1 {
            // Anchored suffix.
            if !seg[pos..].ends_with(part) {
                return false;
            }
        } else if let Some(found) = seg[pos..].find(part) {
            pos += found + part.len();
        } else {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn double_star_crosses_segments() {
        assert!(glob_match("src/**", "src/a/b.rs"));
        assert!(glob_match("src/**", "src/main.rs"));
        assert!(glob_match("src/**", "src")); // zero trailing segments
        assert!(!glob_match("src/**", "tests/a.rs"));
    }

    #[test]
    fn single_star_within_segment() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "main.py"));
        assert!(glob_match("**/*.rs", "a/b/c.rs"));
        assert!(glob_match("services/*/src/**", "services/api/src/lib.rs"));
        assert!(!glob_match("services/*/src/**", "services/api/extra/src/lib.rs"));
    }

    #[test]
    fn literal_match() {
        assert!(glob_match("Cargo.toml", "Cargo.toml"));
        assert!(!glob_match("Cargo.toml", "Cargo.lock"));
    }
}
