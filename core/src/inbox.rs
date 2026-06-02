//! Inbox routing rules. Declarative regex-shaped rules evaluated on
//! filesystem `Created` events for indexable files (`.md` / `.txt`).
//! First match wins; each match can move the file and/or append a tag.
//!
//! See `docs/inbox-rules.md` for the spec.
//
// status: inbox-rules

use std::path::Path;

use regex::Regex;

use crate::config::sections::InboxRule;
use crate::errors::HikerError;

/// Body bytes scanned for the `body` match. Capped to keep the per-file
/// work small even when the user drops a 5 MB log file into the inbox.
pub const BODY_SCAN_BYTES: usize = 4096;

/// Compiled rule list. Built once per vault open from `Config.inbox.rules`
/// and shared via `Arc` to the indexer task.
#[derive(Debug, Clone)]
pub struct Rules {
    compiled: Vec<CompiledRule>,
}

#[derive(Debug, Clone)]
struct CompiledRule {
    basename: Option<Regex>,
    body: Option<Regex>,
    move_to: Option<String>,
    add_tag: Option<String>,
}

impl Rules {
    /// Compile the rule list. Bubbles up validation errors with the
    /// offending rule index so the operator can grep the TOML.
    pub fn compile(rules: &[InboxRule]) -> Result<Self, HikerError> {
        let mut compiled = Vec::with_capacity(rules.len());
        for (idx, rule) in rules.iter().enumerate() {
            compiled.push(compile_one(idx, rule)?);
        }
        Ok(Self { compiled })
    }

    /// Validate without holding the compiled regexes. Used by
    /// `Config::load` so a malformed `[inbox]` block aborts startup with
    /// the same posture as other cross-field checks.
    pub fn validate(rules: &[InboxRule]) -> Result<(), String> {
        for (idx, rule) in rules.iter().enumerate() {
            compile_one(idx, rule).map_err(|e| match e {
                HikerError::Config(s) => s,
                other => other.to_string(),
            })?;
        }
        Ok(())
    }

    /// True if the rule list is empty — the indexer can short-circuit the
    /// Created-event hook entirely in this (very common) case.
    pub const fn is_empty(&self) -> bool {
        self.compiled.is_empty()
    }

    /// Evaluate the rules against a freshly-created file at `rel_path`.
    /// Reads up to `BODY_SCAN_BYTES` of body bytes for body-regex match
    /// evaluation. Returns the (possibly new) `rel_path` plus an
    /// `Applied` record describing what changed, or `None` if no rule
    /// matched. The store / index update is the caller's job — this
    /// helper drives `core::vault::move_note` and the frontmatter
    /// tag-append, and reports back so the caller can route the eventual
    /// upsert to the correct path.
    ///
    /// Errors from the underlying move / write are propagated; rule
    /// evaluation failures (regex non-matches) just fall through.
    pub fn apply_to_created(
        &self,
        vault: &crate::vault::Vault,
        store: &mut crate::store::Store,
        watcher: Option<&crate::watcher::Watcher>,
        rel_path: &str,
    ) -> Result<Option<Applied>, HikerError> {
        if self.compiled.is_empty() {
            return Ok(None);
        }
        let basename = match Path::new(rel_path).file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => return Ok(None),
        };
        let body_sample = read_body_sample(vault, rel_path)?;

        for (idx, rule) in self.compiled.iter().enumerate() {
            if !rule_matches(rule, &basename, body_sample.as_deref()) {
                continue;
            }
            tracing::info!(
                rule_index = idx,
                path = %rel_path,
                move_to = ?rule.move_to,
                add_tag = ?rule.add_tag,
                "inbox: rule matched",
            );
            let mut current = rel_path.to_string();
            let mut moved_to: Option<String> = None;
            if let Some(target_dir) = rule.move_to.as_deref() {
                let new_rel = join_dir_basename(target_dir, &basename);
                if new_rel != current {
                    // Inbox triage moves plain notes; a companion folder is
                    // possible but the per-child reference rewrites ride the
                    // indexer's own move path, so the returned member pairs
                    // are not needed here.
                    let _ = crate::vault::move_note(vault, store, watcher, &current, &new_rel)?;
                    current = new_rel.clone();
                    moved_to = Some(new_rel);
                }
            }
            let mut tagged: Option<String> = None;
            if let Some(tag) = rule.add_tag.as_deref() {
                append_tag(vault, watcher, &current, tag)?;
                tagged = Some(tag.to_string());
            }
            return Ok(Some(Applied {
                rule_index: idx,
                final_rel_path: current,
                moved_to,
                tagged,
            }));
        }
        Ok(None)
    }
}

/// Outcome of a successful rule match. Returned to the caller so a UI
/// surface can render a toast or audit log entry.
#[derive(Debug, Clone)]
pub struct Applied {
    pub rule_index: usize,
    /// Where the file lives after applying the rule. Same as the input
    /// `rel_path` when the rule only added a tag.
    pub final_rel_path: String,
    /// `Some(<new path>)` when the rule moved the file; `None` when the
    /// action was tag-only.
    pub moved_to: Option<String>,
    /// `Some(<tag>)` when the rule appended a tag; `None` when the
    /// action was move-only.
    pub tagged: Option<String>,
}

fn compile_one(idx: usize, rule: &InboxRule) -> Result<CompiledRule, HikerError> {
    let m = &rule.match_;
    let a = &rule.action;
    if m.basename.is_none() && m.body.is_none() {
        return Err(HikerError::Config(format!(
            "rule {idx}: `match` requires at least one of `basename` or `body`"
        )));
    }
    if a.move_to.is_none() && a.add_tag.is_none() {
        return Err(HikerError::Config(format!(
            "rule {idx}: `action` requires at least one of `move_to` or `add_tag`"
        )));
    }
    let basename = match m.basename.as_deref() {
        Some(s) => Some(Regex::new(s).map_err(|e| {
            HikerError::Config(format!("rule {idx}: invalid basename regex: {e}"))
        })?),
        None => None,
    };
    let body = match m.body.as_deref() {
        Some(s) => Some(Regex::new(s).map_err(|e| {
            HikerError::Config(format!("rule {idx}: invalid body regex: {e}"))
        })?),
        None => None,
    };
    if let Some(target) = a.move_to.as_deref() {
        validate_move_target(idx, target)?;
    }
    Ok(CompiledRule {
        basename,
        body,
        move_to: a.move_to.as_ref().map(|s| normalize_dir(s)),
        add_tag: a.add_tag.clone(),
    })
}

fn validate_move_target(idx: usize, target: &str) -> Result<(), HikerError> {
    if target.starts_with('/') {
        return Err(HikerError::Config(format!(
            "rule {idx}: `move_to` must be vault-relative, not absolute (`{target}`)"
        )));
    }
    for comp in target.split('/') {
        if comp == ".." {
            return Err(HikerError::Config(format!(
                "rule {idx}: `move_to` may not contain `..` traversal (`{target}`)"
            )));
        }
    }
    Ok(())
}

fn normalize_dir(target: &str) -> String {
    target.trim_end_matches('/').to_string()
}

fn join_dir_basename(dir: &str, basename: &str) -> String {
    if dir.is_empty() {
        basename.to_string()
    } else {
        format!("{dir}/{basename}")
    }
}

fn rule_matches(rule: &CompiledRule, basename: &str, body: Option<&str>) -> bool {
    if let Some(re) = &rule.basename
        && !re.is_match(basename)
    {
        return false;
    }
    if let Some(re) = &rule.body {
        let Some(body) = body else { return false };
        if !re.is_match(body) {
            return false;
        }
    }
    true
}

fn read_body_sample(
    vault: &crate::vault::Vault,
    rel_path: &str,
) -> Result<Option<String>, HikerError> {
    use std::io::Read;
    let abs = vault.abs_path(rel_path)?;
    let mut f = match std::fs::File::open(&abs) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(HikerError::Io(e.to_string())),
    };
    let mut buf = vec![0u8; BODY_SCAN_BYTES];
    let n = f
        .read(&mut buf)
        .map_err(|e| HikerError::Io(e.to_string()))?;
    buf.truncate(n);
    // Non-UTF-8 inputs lose the body match but a basename-only rule can
    // still fire; return None so `rule_matches` treats a body-required
    // rule as a non-match.
    match String::from_utf8(buf) {
        Ok(s) => Ok(Some(s)),
        Err(_) => Ok(None),
    }
}

/// Append `tag` to the file's frontmatter `tags` list, idempotently.
/// Routes through the same `write_file` path the user save uses so the
/// watcher suppression keeps the resulting Modified event out of the
/// indexer's queue (we re-ingest inline once the rule pass finishes).
fn append_tag(
    vault: &crate::vault::Vault,
    watcher: Option<&crate::watcher::Watcher>,
    rel: &str,
    tag: &str,
) -> Result<(), HikerError> {
    let existing = vault.read_file(rel)?;
    let split = crate::frontmatter::split(&existing);
    let body = split.body.to_string();
    let mut fm = match split.frontmatter {
        Some(serde_yml::Value::Mapping(m)) => m,
        _ => serde_yml::Mapping::new(),
    };
    let mut tags: Vec<String> = match fm.get("tags") {
        Some(serde_yml::Value::Sequence(seq)) => seq
            .iter()
            .filter_map(|v| v.as_str().map(std::string::ToString::to_string))
            .collect(),
        _ => Vec::new(),
    };
    if tags.iter().any(|t| t == tag) {
        return Ok(());
    }
    tags.push(tag.to_string());
    let seq: serde_yml::Sequence = tags
        .into_iter()
        .map(serde_yml::Value::String)
        .collect();
    fm.insert(
        serde_yml::Value::String("tags".into()),
        serde_yml::Value::Sequence(seq),
    );
    let merged = crate::frontmatter::assemble(&serde_yml::Value::Mapping(fm), &body)
        .map_err(|e| HikerError::Io(format!("frontmatter: {e}")))?;
    if let Some(w) = watcher {
        w.suppress(rel.to_string());
    }
    vault.write_file(rel, &merged)?;
    if let Some(w) = watcher {
        w.suppress(rel.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::sections::{InboxAction, InboxMatch};
    use crate::store::Store;
    use crate::vault::Vault;

    struct Fixture {
        _tmp: tempfile::TempDir,
        vault: Vault,
        store: Store,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join(".hiker")).unwrap();
        let vault = Vault::open(&root).unwrap();
        let store = Store::open(&root).unwrap();
        Fixture { _tmp: tmp, vault, store }
    }

    fn write(vault: &Vault, rel: &str, contents: &str) {
        if let Some(parent) = Path::new(rel).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(vault.root().join(parent)).unwrap();
        }
        vault.write_file(rel, contents).unwrap();
    }

    fn rule(basename: Option<&str>, body: Option<&str>, move_to: Option<&str>, add_tag: Option<&str>) -> InboxRule {
        InboxRule {
            match_: InboxMatch {
                basename: basename.map(String::from),
                body: body.map(String::from),
            },
            action: InboxAction {
                move_to: move_to.map(String::from),
                add_tag: add_tag.map(String::from),
            },
        }
    }

    #[test]
    fn basename_match_moves_file() {
        let mut fx = fixture();
        std::fs::create_dir_all(fx.vault.root().join("recipes")).unwrap();
        write(&fx.vault, "inbox/cookies.md", "yum\n");
        let rules = Rules::compile(&[rule(Some(r"\.md$"), None, Some("recipes"), None)]).unwrap();
        let applied = rules
            .apply_to_created(&fx.vault, &mut fx.store, None, "inbox/cookies.md")
            .unwrap()
            .unwrap();
        assert_eq!(applied.final_rel_path, "recipes/cookies.md");
        assert_eq!(applied.moved_to.as_deref(), Some("recipes/cookies.md"));
        assert!(applied.tagged.is_none());
        assert!(!fx.vault.root().join("inbox/cookies.md").exists());
        assert!(fx.vault.root().join("recipes/cookies.md").exists());
    }

    #[test]
    fn body_match_adds_tag() {
        let mut fx = fixture();
        write(&fx.vault, "inbox/note.md", "this is a TODO\nfollow up\n");
        let rules = Rules::compile(&[rule(None, Some("TODO"), None, Some("urgent"))]).unwrap();
        let applied = rules
            .apply_to_created(&fx.vault, &mut fx.store, None, "inbox/note.md")
            .unwrap()
            .unwrap();
        assert_eq!(applied.final_rel_path, "inbox/note.md");
        assert!(applied.moved_to.is_none());
        assert_eq!(applied.tagged.as_deref(), Some("urgent"));
        let after = fx.vault.read_file("inbox/note.md").unwrap();
        assert!(after.contains("tags:"));
        assert!(after.contains("urgent"));
    }

    #[test]
    fn basename_and_body_are_anded() {
        let mut fx = fixture();
        write(&fx.vault, "inbox/journal.md", "boring entry\n");
        write(&fx.vault, "inbox/journal2.md", "today TODO buy milk\n");
        let rules = Rules::compile(&[rule(
            Some(r"^journal"),
            Some("TODO"),
            None,
            Some("flagged"),
        )])
        .unwrap();
        // First file: basename matches, body does NOT → no fire.
        let a = rules
            .apply_to_created(&fx.vault, &mut fx.store, None, "inbox/journal.md")
            .unwrap();
        assert!(a.is_none(), "AND requires both predicates to hold");
        // Second file: both match → fires.
        let b = rules
            .apply_to_created(&fx.vault, &mut fx.store, None, "inbox/journal2.md")
            .unwrap();
        assert!(b.is_some());
    }

    #[test]
    fn first_match_wins() {
        let mut fx = fixture();
        std::fs::create_dir_all(fx.vault.root().join("first")).unwrap();
        std::fs::create_dir_all(fx.vault.root().join("second")).unwrap();
        write(&fx.vault, "inbox/foo.md", "x\n");
        let rules = Rules::compile(&[
            rule(Some(r"\.md$"), None, Some("first"), None),
            rule(Some(r"\.md$"), None, Some("second"), None),
        ])
        .unwrap();
        let applied = rules
            .apply_to_created(&fx.vault, &mut fx.store, None, "inbox/foo.md")
            .unwrap()
            .unwrap();
        assert_eq!(applied.rule_index, 0);
        assert!(fx.vault.root().join("first/foo.md").exists());
        assert!(!fx.vault.root().join("second/foo.md").exists());
    }

    #[test]
    fn no_match_leaves_file_alone() {
        let mut fx = fixture();
        write(&fx.vault, "inbox/keep.md", "nothing special\n");
        let rules = Rules::compile(&[rule(Some(r"^never_matches$"), None, Some("nope"), None)]).unwrap();
        let applied = rules
            .apply_to_created(&fx.vault, &mut fx.store, None, "inbox/keep.md")
            .unwrap();
        assert!(applied.is_none());
        assert!(fx.vault.root().join("inbox/keep.md").exists());
    }

    #[test]
    fn invalid_regex_is_rejected() {
        let bad = rule(Some("("), None, Some("dest"), None);
        let err = Rules::compile(&[bad]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("rule 0"), "got: {msg}");
        assert!(msg.contains("basename regex"), "got: {msg}");
    }

    #[test]
    fn missing_action_is_rejected() {
        let bad = InboxRule {
            match_: InboxMatch {
                basename: Some(r"\.md$".to_string()),
                body: None,
            },
            action: InboxAction::default(),
        };
        let err = Rules::compile(&[bad]).unwrap_err();
        assert!(err.to_string().contains("at least one of `move_to` or `add_tag`"));
    }

    #[test]
    fn missing_match_is_rejected() {
        let bad = InboxRule {
            match_: InboxMatch::default(),
            action: InboxAction {
                move_to: Some("dest".to_string()),
                add_tag: None,
            },
        };
        let err = Rules::compile(&[bad]).unwrap_err();
        assert!(err.to_string().contains("at least one of `basename` or `body`"));
    }

    #[test]
    fn move_to_rejects_parent_traversal() {
        let bad = rule(Some(r"\.md$"), None, Some("../escape"), None);
        let err = Rules::compile(&[bad]).unwrap_err();
        assert!(err.to_string().contains(".."));
    }

    #[test]
    fn move_to_rejects_absolute_path() {
        let bad = rule(Some(r"\.md$"), None, Some("/etc"), None);
        let err = Rules::compile(&[bad]).unwrap_err();
        assert!(err.to_string().contains("vault-relative"));
    }
}
