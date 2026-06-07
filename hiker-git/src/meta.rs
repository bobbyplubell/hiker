//! Plain-Rust commit value types crossing the [`crate::repo::GitBackend`] boundary.
//!
//! These mirror the `.ops` frame's attribution (`op-log-attribution`) so a git
//! commit and its frame agree by construction: the `Hiker-Author` trailer
//! carries the frame's `Author` class, and an observed move adds a
//! `Hiker-Rename` trailer (`git-attribution-trailer`, `git-observed-rename-
//! commit`). No `git2` type appears here — this is the seam's data side.

use std::fmt;

/// Authorship class mirroring the `.ops` frame's `Author` (`op-log-attribution`).
/// Rendered into the `Hiker-Author:` commit trailer verbatim, so the activity
/// feed projection from `git log` + trailers agrees with the one from `.ops`.
///
/// The wire forms match the spec's trailer grammar:
/// `user | agent:<id> | external | extractor:<id> | auto:<id> | sync:<device>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Author {
    /// The user typed it.
    User,
    /// An agent session authored it; `id` is the session/agent identifier.
    Agent(String),
    /// An external edit folded in (a disk edit from another editor / a git
    /// commit hook / a user amend reconciled on the next pass).
    External,
    /// An extractor produced it (a re-extraction overwriting a sidecar body).
    Extractor(String),
    /// An automatic non-agent process (inbox routing, enrichment) authored it.
    Auto(String),
    /// A synced change from another device; `device` is the source fingerprint
    /// or human name.
    Sync(String),
}

impl Author {
    /// The trailer-value string for this class, matching the `git.md` grammar.
    /// This is the canonical encoding both `render`/`parse` use.
    #[must_use]
    pub fn trailer_value(&self) -> String {
        match self {
            Author::User => "user".to_string(),
            Author::Agent(id) => format!("agent:{id}"),
            Author::External => "external".to_string(),
            Author::Extractor(id) => format!("extractor:{id}"),
            Author::Auto(id) => format!("auto:{id}"),
            Author::Sync(device) => format!("sync:{device}"),
        }
    }

    /// Parse a `Hiker-Author` trailer value back into the class. Unknown forms
    /// fall back to [`Author::External`] (the safest "came from outside" bucket)
    /// rather than erroring, so an unrecognized trailer never breaks a read.
    #[must_use]
    pub fn parse_trailer_value(s: &str) -> Self {
        let s = s.trim();
        if let Some(id) = s.strip_prefix("agent:") {
            Author::Agent(id.to_string())
        } else if let Some(id) = s.strip_prefix("extractor:") {
            Author::Extractor(id.to_string())
        } else if let Some(id) = s.strip_prefix("auto:") {
            Author::Auto(id.to_string())
        } else if let Some(d) = s.strip_prefix("sync:") {
            Author::Sync(d.to_string())
        } else if s == "user" {
            Author::User
        } else {
            Author::External
        }
    }
}

impl fmt::Display for Author {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.trailer_value())
    }
}

/// The hiker-specific commit trailers (`git-attribution-trailer`). `author` is
/// always present; `rename` is set only on an observed-move commit
/// (`git-observed-rename-commit`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trailers {
    /// The `Hiker-Author:` value — the change's authorship class.
    pub author: Author,
    /// The `Hiker-Rename: <from> -> <to>` move, when this is a rename commit.
    pub rename: Option<(String, String)>,
}

/// Trailer key for the authorship class.
pub const TRAILER_AUTHOR: &str = "Hiker-Author";
/// Trailer key for an observed move.
pub const TRAILER_RENAME: &str = "Hiker-Rename";

impl Trailers {
    /// A plain authored-change trailer set (no rename).
    #[must_use]
    pub const fn authored(author: Author) -> Self {
        Self { author, rename: None }
    }

    /// A rename-commit trailer set carrying both the author and the move.
    #[must_use]
    pub const fn renamed(author: Author, from: String, to: String) -> Self {
        Self { author, rename: Some((from, to)) }
    }

    /// Render the trailers as the lines appended to a commit message body,
    /// separated from the subject/body by a blank line per git trailer
    /// convention. The returned string starts with `\n\n` so it appends cleanly
    /// onto a subject line.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!("\n\n{TRAILER_AUTHOR}: {}", self.author.trailer_value());
        if let Some((from, to)) = &self.rename {
            out.push_str(&format!("\n{TRAILER_RENAME}: {from} -> {to}"));
        }
        out
    }

    /// Parse the hiker trailers out of a full commit message. Missing
    /// `Hiker-Author` defaults to [`Author::External`] (a commit hiker didn't
    /// write — e.g. a user's own commit — is an external change by
    /// classification). The `Hiker-Rename` `from -> to` is recovered when
    /// present.
    #[must_use]
    pub fn parse(message: &str) -> Self {
        let mut author = Author::External;
        let mut rename = None;
        for line in message.lines() {
            if let Some(v) = line.strip_prefix(&format!("{TRAILER_AUTHOR}:")) {
                author = Author::parse_trailer_value(v);
            } else if let Some(v) = line.strip_prefix(&format!("{TRAILER_RENAME}:"))
                && let Some((from, to)) = v.split_once("->")
            {
                rename = Some((from.trim().to_string(), to.trim().to_string()));
            }
        }
        Self { author, rename }
    }
}

/// One commit as the seam reports it for inspection (`git log` / `git show`
/// projection — `git-parallel-history`). Plain data; no `git2` type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    /// The full commit sha (hex).
    pub sha: String,
    /// The commit subject (first line of the message).
    pub subject: String,
    /// Author name as git recorded it.
    pub author_name: String,
    /// Commit time, seconds since the unix epoch.
    pub time_unix: i64,
    /// The parsed hiker trailers (`Hiker-Author` / `Hiker-Rename`).
    pub trailers: Trailers,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn author_round_trips_through_trailer_value() {
        let cases = [
            Author::User,
            Author::Agent("sess-1".into()),
            Author::External,
            Author::Extractor("pdf".into()),
            Author::Auto("inbox".into()),
            Author::Sync("laptop".into()),
        ];
        for a in cases {
            let v = a.trailer_value();
            assert_eq!(Author::parse_trailer_value(&v), a, "round-trip for {v}");
        }
    }

    #[test]
    fn unknown_author_value_falls_back_to_external() {
        assert_eq!(Author::parse_trailer_value("nonsense:42"), Author::External);
    }

    #[test]
    fn trailers_render_and_parse_round_trip() {
        let t = Trailers::authored(Author::Agent("a1".into()));
        let msg = format!("did a thing{}", t.render());
        assert_eq!(Trailers::parse(&msg), t);
    }

    #[test]
    fn rename_trailer_round_trips() {
        let t = Trailers::renamed(Author::User, "old/note.md".into(), "new/note.md".into());
        let msg = format!("moved note{}", t.render());
        let parsed = Trailers::parse(&msg);
        assert_eq!(parsed.author, Author::User);
        assert_eq!(parsed.rename, Some(("old/note.md".into(), "new/note.md".into())));
    }

    #[test]
    fn missing_author_trailer_parses_as_external() {
        let parsed = Trailers::parse("a user's own commit, no trailers\n");
        assert_eq!(parsed.author, Author::External);
        assert_eq!(parsed.rename, None);
    }
}
