//! Properties tab body. Shows filesystem + content metadata for a note:
//! basename, full path, size, mtime, word/line counts, link counts, and
//! frontmatter key/value pairs.

use std::path::Path;
use std::time::SystemTime;

use eframe::egui;

use crate::state::AppState;
use hiker_theme as theme;

pub fn show(ui: &mut egui::Ui, app: &mut AppState, path: &str) {
    let basename = path.rsplit('/').next().unwrap_or(path).to_string();
    ui.heading(format!("Properties · {}", basename));
    ui.add_space(8.0);

    let abs = app.vault_session.vault_root.join(path);
    let meta = std::fs::metadata(&abs).ok();

    // Indexer-backed metadata: pulled from the read store + layered doc so we
    // surface the same fields the legacy `note_properties` command did
    // (note id, content hash, indexed_at, embedder version, chunk count,
    // last_accessed_at, change count, skipped state). The read store may be
    // offline (vault not yet indexed) in which case the rows below are
    // simply omitted.
    let props: Option<hiker_core::store::dto::NoteProperties> = app
        .vault_session.services.read_store
        .lock()
        .ok()
        .and_then(|s| s.note_properties(path).ok().flatten());
    let change_count: Option<i64> = hiker_core::ops::op_writes::snapshot_history(
        &app.vault_session.services.layered,
        path,
        usize::MAX,
    )
    .ok()
    .map(|rows| rows.len() as i64);

    // Resolve buffer body or read from disk as a fallback.
    let body = match app.session.buffers.get(path) {
        Some(b) => b.current_text(),
        None => app.vault_session.vault.read_file(path).unwrap_or_default(),
    };

    let view = View { app };

    egui::Grid::new("props-fs")
        .num_columns(2)
        .spacing(egui::vec2(20.0, 6.0))
        .show(ui, |ui| {
            ui.label("basename:");
            ui.label(&basename);
            ui.end_row();

            ui.label("path:");
            ui.label(egui::RichText::new(path).monospace());
            ui.end_row();

            if let Some(m) = &meta {
                ui.label("size:");
                ui.label(format!("{} bytes", m.len()));
                ui.end_row();

                ui.label("modified:");
                let s = m
                    .modified()
                    .ok()
                    .map(|t| view.format_systime(t))
                    .unwrap_or_else(|| "(unknown)".to_string());
                ui.label(s);
                ui.end_row();
            } else {
                ui.label("");
                ui.label(
                    egui::RichText::new("(file not found on disk)")
                        .color(theme::muted()),
                );
                ui.end_row();
            }

            let wc = body.split_whitespace().count();
            let lc = if body.is_empty() {
                0
            } else {
                body.lines().count()
            };
            ui.label("words:");
            ui.label(format!("{}", wc));
            ui.end_row();

            ui.label("lines:");
            ui.label(format!("{}", lc));
            ui.end_row();

            let outgoing = view.count_outgoing_wikilinks(&body);
            ui.label("outgoing links:");
            ui.label(format!("{}", outgoing));
            ui.end_row();

            let (inbound, capped) = view.estimate_inbound(&basename);
            ui.label("inbound links:");
            ui.label(if capped {
                format!(">={}", inbound)
            } else {
                format!("{}", inbound)
            });
            ui.end_row();

            // Indexer-backed rows. Each is rendered only when the
            // corresponding field is populated, so an un-indexed note
            // shows a stripped-down panel rather than a wall of `(none)`.
            if let Some(p) = props.as_ref() {
                if let Some(nid) = &p.note_id {
                    ui.label("note id:");
                    ui.label(egui::RichText::new(nid).monospace());
                    ui.end_row();
                }
                if let Some(h) = &p.content_hash {
                    ui.label("content hash:");
                    ui.label(egui::RichText::new(view.short_hash(h)).monospace());
                    ui.end_row();
                }
                if let Some(c) = p.chunk_count {
                    ui.label("chunks:");
                    ui.label(format!("{}", c));
                    ui.end_row();
                }
                if let Some(ts) = p.indexed_at {
                    ui.label("indexed at:");
                    ui.label(format_unix_ms(ts));
                    ui.end_row();
                }
                if let Some(ts) = p.last_accessed_at {
                    ui.label("last accessed:");
                    ui.label(format_unix_ms(ts));
                    ui.end_row();
                }
                if let Some(ver) = &p.embedder_version
                    && !ver.is_empty()
                {
                    ui.label("embedder:");
                    ui.label(egui::RichText::new(ver).monospace());
                    ui.end_row();
                }
                if matches!(p.skipped, Some(true)) {
                    ui.label("skipped:");
                    let reason = p
                        .skip_reason
                        .clone()
                        .unwrap_or_else(|| "(no reason recorded)".into());
                    ui.label(
                        egui::RichText::new(reason).color(theme::muted()),
                    );
                    ui.end_row();
                }
            }
            if let Some(n) = change_count {
                ui.label("changes:");
                ui.label(format!("{}", n));
                ui.end_row();
            }
        });

    // Frontmatter section. Only render if we parsed at least one key.
    if let Some(pairs) = view.parse_frontmatter(&body)
        && !pairs.is_empty()
    {
        ui.add_space(12.0);
        ui.label(egui::RichText::new("Frontmatter").strong());
        egui::Grid::new("props-fm")
            .num_columns(2)
            .spacing(egui::vec2(20.0, 4.0))
            .show(ui, |ui| {
                for (k, v) in pairs {
                    ui.label(egui::RichText::new(k).monospace());
                    ui.label(egui::RichText::new(v).monospace());
                    ui.end_row();
                }
            });
    }

    render_epic_progress(ui, app, path);

    // Trail membership: list every trail that contains this note as a
    // waypoint, via the derived `trail_waypoints` reverse lookup. Per
    // `properties` spec — surfaces the note's role in user-curated
    // structure.
    let trail_hits: Vec<String> = {
        match app.vault_session.services.read_store.lock() {
            Ok(store) => hiker_core::trails::containing_note_with_paths(
                &app.vault_session.vault,
                &store,
                &app.vault_session.services.layered,
                path,
            )
            .unwrap_or_default()
            .into_iter()
            .map(|h| {
                let base = h.trail_doc_rel.rsplit('/').next().unwrap_or(&h.trail_doc_rel);
                base.strip_suffix(".md").unwrap_or(base).to_string()
            })
            .collect(),
            Err(_) => Vec::new(),
        }
    };
    if !trail_hits.is_empty() {
        ui.add_space(12.0);
        ui.label(egui::RichText::new("Trails").strong());
        egui::Grid::new("props-trails")
            .num_columns(2)
            .spacing(egui::vec2(20.0, 4.0))
            .show(ui, |ui| {
                for name in &trail_hits {
                    ui.label(egui::RichText::new(name).monospace());
                    ui.label("waypoint");
                    ui.end_row();
                }
            });
    }

    // Cluster membership: walk the persisted trees and surface the
    // (tree, cluster) pairs this note belongs to via its leaf rows.
    let cluster_hits = view.cluster_memberships(path);
    if !cluster_hits.is_empty() {
        ui.add_space(12.0);
        ui.label(egui::RichText::new("Clusters").strong());
        egui::Grid::new("props-clusters")
            .num_columns(2)
            .spacing(egui::vec2(20.0, 4.0))
            .show(ui, |ui| {
                for hit in &cluster_hits {
                    ui.label(egui::RichText::new(hit.tree_name.as_str()).monospace());
                    ui.label(hit.cluster_label.as_str());
                    ui.end_row();
                }
            });
    }
}

/// Epic / plan rollup section (`pm-epic-rollup`'s properties-panel
/// surface): a list-like note's progress, derived fresh from the boards on
/// render — per-category counts + estimate sums over its members' derived
/// statuses, never stored. Renders nothing for every other kind.
fn render_epic_progress(ui: &mut egui::Ui, app: &AppState, path: &str) {
    let Some(progress) = epic_progress_of(app, path) else { return };
    ui.add_space(12.0);
    ui.label(egui::RichText::new("Progress").strong());
    egui::Grid::new("props-epic")
        .num_columns(2)
        .spacing(egui::vec2(20.0, 4.0))
        .show(ui, |ui| {
            ui.label("members:");
            ui.label(progress.summary());
            ui.end_row();
            let categories = [
                ("backlog", &progress.backlog),
                ("todo", &progress.todo),
                ("in progress", &progress.in_progress),
                ("done", &progress.done),
                ("canceled", &progress.canceled),
            ];
            for (name, tally) in categories {
                if tally.count == 0 {
                    continue;
                }
                ui.label(format!("{name}:"));
                if tally.estimate > 0.0 {
                    ui.label(format!("{} (est {})", tally.count, tally.estimate));
                } else {
                    ui.label(format!("{}", tally.count));
                }
                ui.end_row();
            }
            if progress.conflicted > 0 {
                ui.label("conflicted:");
                ui.label(
                    egui::RichText::new(format!("{} (on 2+ sprints)", progress.conflicted))
                        .color(theme::warn()),
                );
                ui.end_row();
            }
        });
}

/// The epic rollup for `path` when it is a registered list-like note
/// (`pm-epic-rollup`'s properties-panel surface): `None` for every other
/// kind, when the store is offline, or when the rollup read fails.
fn epic_progress_of(app: &AppState, path: &str) -> Option<hiker_core::pm::EpicProgress> {
    let registry = app.vault_session.services.kinds.as_ref();
    let store = app.vault_session.services.read_store.lock().ok()?;
    let kind = store.meta_value(path, "hiker.kind").ok().flatten()?;
    registry.list_like(&kind)?;
    hiker_core::pm::epic_progress(&store, registry, path).ok()
}

struct ClusterMembership {
    tree_name: String,
    cluster_label: String,
}

/// Read-only render context for the properties tab. Bundles `&AppState`
/// so the metadata helpers are inherent methods rather than a row of
/// single-use free functions.
struct View<'a> {
    app: &'a AppState,
}

impl View<'_> {
    fn cluster_memberships(&self, path: &str) -> Vec<ClusterMembership> {
    use hiker_core::trees::types::NodeKind;
    let app = self.app;
    let trees = app.vault_session.services.trees.as_ref();
    // Path-as-identity: leaves carry the note's rel-path directly, so we
    // join on `path` without any doc-id resolution.
    let mut out: Vec<ClusterMembership> = Vec::new();
    let Ok(tree_list) = trees.list_trees() else {
        return out;
    };
    for tree in tree_list {
        let Ok(nodes) = trees.list_nodes(&tree.id) else {
            continue;
        };
        let nodes_by_id: std::collections::HashMap<&str, &hiker_core::trees::types::EditableNode> =
            nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        for n in &nodes {
            if !matches!(n.kind, NodeKind::Leaf) {
                continue;
            }
            if n.note_path.as_deref() != Some(path) {
                continue;
            }
            let mut chain: Vec<String> = Vec::new();
            let mut cur = n.parent.as_deref();
            while let Some(pid) = cur {
                let Some(parent) = nodes_by_id.get(pid) else { break };
                chain.push(parent.name.clone());
                cur = parent.parent.as_deref();
            }
            chain.reverse();
            let label = if chain.is_empty() {
                "(root)".to_string()
            } else {
                chain.join(" /")
            };
            out.push(ClusterMembership {
                tree_name: tree.name.clone(),
                cluster_label: label,
            });
        }
    }
    out
    }

    fn short_hash(&self, h: &str) -> String {
        let n = h.len().min(16);
        h[..n].to_string()
    }
}

/// Format a Unix epoch value as a local date-time string. The legacy DTO
/// returns *milliseconds*, but `indexed_at` and `last_accessed_at` are
/// stored as seconds in the notes table — try seconds first, fall back to
/// milliseconds if the seconds interpretation lands implausibly far out
/// (more than ~3000 AD).
fn format_unix_ms(t: i64) -> String {
    use time::OffsetDateTime;
    use time::macros::format_description;
    let secs = if t.abs() > 100_000_000_000 { t / 1000 } else { t };
    let Ok(odt) = OffsetDateTime::from_unix_timestamp(secs) else {
        return "(invalid time)".to_string();
    };
    let fmt = format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    odt.format(fmt).unwrap_or_else(|_| "(format failed)".to_string())
}

impl View<'_> {
    fn format_systime(&self, t: SystemTime) -> String {
    use time::OffsetDateTime;
    use time::macros::format_description;
    let Ok(d) = t.duration_since(SystemTime::UNIX_EPOCH) else {
        return "(invalid time)".to_string();
    };
    let Ok(odt) = OffsetDateTime::from_unix_timestamp(d.as_secs() as i64) else {
        return "(invalid time)".to_string();
    };
    let fmt = format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    odt.format(fmt).unwrap_or_else(|_| "(format failed)".to_string())
    }

    /// Count `[[…]]` wikilinks in `body`. Mirrors the regex
    /// `\[\[([^\]|]+)` but written by hand since `app` has no `regex` dep.
    fn count_outgoing_wikilinks(&self, body: &str) -> usize {
    let bytes = body.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            // Look for the closing `]` or `|`. Empty target doesn't count.
            let mut j = i + 2;
            let mut has_chars = false;
            while j < bytes.len() {
                let c = bytes[j];
                if c == b']' || c == b'|' || c == b'\n' {
                    break;
                }
                has_chars = true;
                j += 1;
            }
            if has_chars {
                count += 1;
            }
            i = j.max(i + 2);
        } else {
            i += 1;
        }
    }
    count
    }

    /// Scan the vault for `[[basename]]` references. Bounded effort — we stop
    /// after `MAX_FILES` files or `MAX_HITS` matches and signal "capped" so the
    /// caller can render `≥N`. The basename is matched without its `.md` ext
    /// since wikilinks are typically extensionless.
    fn estimate_inbound(&self, basename: &str) -> (usize, bool) {
    let app = self.app;
    const MAX_FILES: usize = 500;
    const MAX_HITS: usize = 50;

    let stem = basename.strip_suffix(".md").unwrap_or(basename);
    let needle = format!("[[{}", stem);

    let mut hits = 0;
    let mut scanned = 0;
    let mut capped = false;

    // Recursive walk via walkdir-style stack; we use std::fs to avoid a
    // new dep. Skip dotfiles + non-md.
    let root = app.vault_session.vault.root().to_path_buf();
    let mut stack: Vec<std::path::PathBuf> = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            let name = entry.file_name();
            let name_s = name.to_string_lossy();
            if name_s.starts_with('.') {
                continue;
            }
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(p);
                continue;
            }
            if !self.is_md(&p) {
                continue;
            }
            scanned += 1;
            if scanned > MAX_FILES {
                capped = true;
                return (hits, capped);
            }
            if let Ok(text) = std::fs::read_to_string(&p)
                && let Some(pos) = text.find(&needle)
            {
                // Ensure character after the needle is `|`, `]`, or `#` so
                // we don't match a longer wikilink prefix.
                let tail = &text[pos + needle.len()..];
                let ok = tail
                    .chars()
                    .next()
                    .map(|c| c == ']' || c == '|' || c == '#')
                    .unwrap_or(false);
                if ok {
                    hits += 1;
                    if hits >= MAX_HITS {
                        capped = true;
                        return (hits, capped);
                    }
                }
            }
        }
    }
    (hits, capped)
    }

    fn is_md(&self, p: &Path) -> bool {
        p.extension()
            .map(|e| e.eq_ignore_ascii_case("md"))
            .unwrap_or(false)
    }

    /// Parse a YAML frontmatter block at the head of `body`, formatted as
    /// `---\n…\n---\n`. Returns a flat list of stringified key/value pairs;
    /// nested structures are rendered via `serde_yml` default formatting.
    fn parse_frontmatter(&self, body: &str) -> Option<Vec<(String, String)>> {
    let rest = body.strip_prefix("---\n")?;
    let end = rest.find("\n---\n").or_else(|| rest.find("\n---"))?;
    let yaml = &rest[..end];
    let value: serde_yml::Value = serde_yml::from_str(yaml).ok()?;
    let mapping = value.as_mapping()?;
    let mut out = Vec::new();
    for (k, v) in mapping {
        let k_str = match k {
            serde_yml::Value::String(s) => s.clone(),
            _ => serde_yml::to_string(k).unwrap_or_default().trim().to_string(),
        };
        let v_str = match v {
            serde_yml::Value::String(s) => s.clone(),
            serde_yml::Value::Bool(b) => b.to_string(),
            serde_yml::Value::Number(n) => n.to_string(),
            serde_yml::Value::Null => "~".to_string(),
            other => serde_yml::to_string(other).unwrap_or_default().trim().to_string(),
        };
        out.push((k_str, v_str));
    }
    Some(out)
    }
}
