//! Governance + git-change DATA and policy for the unified entity graph — no overlays. The
//! entity graph encodes everything on direct channels (fill = kind, edge = governance drift,
//! node ring = git change, badge = spec status / open bugs); this module supplies the data those
//! channels read and the node context menu:
//!
//! - [`GovCache`] — the lazily-loaded spec-governance rollup (`Governance`), its `links.json`
//!   presence check, and the drift `gov_color`s the `Governs` edges use.
//! - [`Changes`] — the HEAD-vs-worktree change set (file grain, refined to symbol grain), turned
//!   into the per-node **ring** stroke (`code-graph-diff-symbol-level`, now a direct mark not a
//!   recolor) and the "only changed" lens predicate.
//! - [`node_menu`] — the read-only node context menu (Open source / Open diff / select a governing
//!   spec) (`code-graph-open-diff-from-node`, `interaction.md` [rightclick-menu-always]).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use eframe::egui;
use egui_workbench::menu::{Action, Enabled, Menu};

use hiker_code::governance::Governance;
use hiker_code::{GovState, ScipAdapter};
use hiker_git::repo::ChangeStatus;
use spec_engine::SourceId;

/// Governance fill palette: calm green / amber / red for the linked states, and the muted gray
/// for the ungoverned/"not this overlay's story" mass.
const GOV_OK: egui::Color32 = egui::Color32::from_rgb(0x3d, 0x9a, 0x5f);
const GOV_DRIFTED: egui::Color32 = egui::Color32::from_rgb(0xd9, 0x9a, 0x2b);
const GOV_MISSING: egui::Color32 = egui::Color32::from_rgb(0xc8, 0x45, 0x4f);
/// The muted "ungoverned" tone — also the neutral a `Governs` edge to ungoverned code takes.
const MUTED_MASS: egui::Color32 = egui::Color32::from_rgb(0x55, 0x59, 0x60);
/// A body-unchanged symbol in a changed file keeps its file's change color at this reduced
/// strength — still in the change story, but quieter than a body-changed node.
/// status: code-graph-diff-symbol-level
const DIFF_BODY_SAME_DIM: f32 = 0.5;

/// Fill for a folded governance state — the `Governs` edge / coverage-count color.
pub const fn gov_color(state: GovState) -> egui::Color32 {
    match state {
        GovState::Ok => GOV_OK,
        GovState::Drifted => GOV_DRIFTED,
        GovState::Missing => GOV_MISSING,
        GovState::Ungoverned => MUTED_MASS,
    }
}

/// The lazily-loaded spec-governance rollup behind the entity graph: the spec layer's source of
/// `targets_of`/`status_of`/`specs`, the `Governs` edges' drift colors, and the node menu's
/// governing-spec list. Loaded once on the first need (a full drift pass over `links.json`).
#[derive(Default)]
pub struct GovCache {
    /// Drift rollup + doc statuses; `None` until first needed, `Err` = load failure.
    gov: Option<Result<Governance, String>>,
    /// Cached `links.json`-exists check (a per-frame stat would be waste).
    links_present: Option<bool>,
}

impl GovCache {
    /// Whether the repo carries a `links.json` drift baseline (cached stat) — gates building the
    /// spec layer (no baseline → no governance → spec-anchor nodes only).
    pub fn links_present(&mut self, repo_root: &Path) -> bool {
        *self.links_present.get_or_insert_with(|| repo_root.join("links.json").exists())
    }

    /// Load the governance rollup once (drift-checks every linked body — a real one-time cost on
    /// big stores, paid on the first need).
    pub fn ensure(&mut self, adapter: &ScipAdapter, src: &SourceId) {
        if self.gov.is_none() {
            let repo_root = adapter.repo_root();
            self.gov = Some(
                Governance::load(repo_root, &repo_root.join("docs"), src, adapter)
                    .map_err(|e| format!("links.json: {e}")),
            );
        }
    }

    /// The loaded governance rollup, if the load succeeded.
    pub fn governance(&self) -> Option<&Governance> {
        self.gov.as_ref().and_then(|g| g.as_ref().ok())
    }
}

/// The HEAD-vs-worktree change set for a repo, turned into the entity graph's direct **change
/// ring**: a changed code node is stroked in its change color (added/modified/deleted), full when
/// its definition body actually changed, dim when the file merely churned around an unchanged body
/// (`code-graph-diff-symbol-level`). Also the "only changed" lens predicate ([`Changes::touches`]).
pub struct Changes {
    /// Repo-relative changed files → status.
    files: HashMap<String, ChangeStatus>,
    /// Symbol-grain refinement: per refined changed file, the monikers whose body actually differs
    /// vs HEAD. A file with no entry couldn't be refined → every node keeps the full-strength ring.
    sym: HashMap<String, HashSet<String>>,
}

impl Changes {
    /// Load the change set: `git diff HEAD` (vault-relative) → repo-relative, then the symbol-grain
    /// refinement pass. The data the change ring + "only changed" lens read.
    pub fn load(
        git: &crate::git_sync::GitSyncEngine,
        adapter: &ScipAdapter,
        vault_root: &Path,
    ) -> Result<Self, String> {
        let rows = git.diff_paths("HEAD", None).map_err(|e| format!("git diff: {e}"))?;
        let files = rows_to_repo(rows, vault_root, adapter.repo_root());
        let sym = refine_symbol_diff(&files, adapter, git, vault_root);
        Ok(Self { files, sym })
    }

    /// Whether the entity at `file`/`id` changed vs HEAD (the "only changed" lens predicate): its
    /// file changed, and — when the file was refined — its body is among the changed symbols.
    pub fn touches(&self, file: &str, id: &str) -> bool {
        self.files.contains_key(file)
            && self.sym.get(file).is_none_or(|changed| changed.contains(id))
    }

    /// The change ring for a node, or `None` when its file is unchanged. Full stroke = body changed
    /// (or the file couldn't be refined); dim stroke = file churned around an unchanged body.
    pub fn ring(&self, file: &str, id: &str) -> Option<egui::Stroke> {
        let status = *self.files.get(file)?;
        let color = super::git_diff::status_glyph(status).1;
        if self.sym.get(file).is_some_and(|changed| !changed.contains(id)) {
            Some(egui::Stroke::new(1.5, color.gamma_multiply(DIFF_BODY_SAME_DIM)))
        } else {
            Some(egui::Stroke::new(2.0, color))
        }
    }
}

/// Map the vault git engine's changed rows (vault-relative) into repo-relative paths under
/// `repo_root` — rows outside the repo are dropped. The repo root was resolved against the vault
/// root at bind, so prefix-stripping is exact. status: code-graph-diff-coloring
pub(crate) fn rows_to_repo(
    rows: Vec<(String, ChangeStatus)>,
    vault_root: &Path,
    repo_root: &Path,
) -> HashMap<String, ChangeStatus> {
    rows.into_iter()
        .filter_map(|(p, s)| {
            let abs = vault_root.join(&p);
            let rel = abs.strip_prefix(repo_root).ok()?;
            Some((rel.to_string_lossy().into_owned(), s))
        })
        .collect()
}

/// Symbol-grain refinement (`code-graph-diff-symbol-level`): for each `Modified` file, fetch its
/// HEAD text (`git show HEAD:<path>`) and ask the adapter which definition bodies actually differ
/// (name-anchored AST fingerprints). Only files the refinement can *prove* something about get an
/// entry; everything else (added/deleted/renamed, no HEAD text, no grammar) keeps the full ring.
fn refine_symbol_diff(
    diff: &HashMap<String, ChangeStatus>,
    adapter: &ScipAdapter,
    git: &crate::git_sync::GitSyncEngine,
    vault_root: &Path,
) -> HashMap<String, HashSet<String>> {
    let Ok(prefix) = adapter.repo_root().strip_prefix(vault_root) else {
        return HashMap::new(); // repo root resolved against the vault at bind; refuse otherwise
    };
    let mut out = HashMap::new();
    for (file, status) in diff {
        if *status != ChangeStatus::Modified {
            continue;
        }
        let vault_rel = prefix.join(file).to_string_lossy().into_owned();
        let Ok(Some(head_text)) = git.show_at("HEAD", &vault_rel) else { continue };
        if let Some(changed) = adapter.changed_symbols_vs(file, &head_text) {
            out.insert(file.clone(), changed);
        }
    }
    out
}

/// Governance counts over a node-id iterator: `[ok, drifted, missing, ungoverned]` — the summary
/// line's "the ungoverned mass, numerically".
pub fn gov_counts<'a>(gov: &Governance, ids: impl Iterator<Item = &'a str>) -> [usize; 4] {
    let mut out = [0usize; 4];
    for id in ids {
        let i = match gov.state_of(id) {
            GovState::Ok => 0,
            GovState::Drifted => 1,
            GovState::Missing => 2,
            GovState::Ungoverned => 3,
        };
        out[i] += 1;
    }
    out
}

/// A verb picked from the node context menu, applied by the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeAction {
    /// Open the node's source file read-only.
    OpenSource,
    /// Open the node's file with the `GitRef` diff overlay vs HEAD.
    OpenDiff,
    /// Select (focus) this governing spec's node in the graph — the direct replacement for the old
    /// "light spec" overlay verb.
    SelectSpec(String),
    /// Select this CODE node AND set the focus-spotlight hop radius (1/2/3) for the overview, so the
    /// spotlight brightens it + every neighbour within N undirected hops. status: code-graph
    FocusHops(u8),
}

/// Why the "Open diff" verb is or isn't available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffVerb {
    /// Git is enabled — the diff tab can open.
    Ready,
    /// No git engine for this vault.
    NoGit,
}

/// The node's right-click menu (`interaction.md` [rightclick-menu-always]): Open source / Open diff
/// (greyed with the reason when unavailable) / Copy symbol, plus a "Select spec" section listing
/// the node's governing specs. status: code-graph-open-diff-from-node
pub fn node_menu(moniker: &str, diff: DiffVerb, specs: &[String]) -> Menu<NodeAction> {
    let open_diff = match diff {
        DiffVerb::Ready => Action::new("Open diff vs HEAD", NodeAction::OpenDiff),
        DiffVerb::NoGit => Action::new("Open diff vs HEAD", NodeAction::OpenDiff)
            .enabled(Enabled::No("git isn't enabled for this vault".into())),
    };
    let copy = moniker.to_owned();
    let mut menu = Menu::new()
        .action("Open source", NodeAction::OpenSource)
        .action_with(open_diff)
        .custom(move |ui| {
            if ui.button("Copy symbol").clicked() {
                ui.ctx().copy_text(copy.clone());
                ui.close();
            }
            None
        });
    // Focus-spotlight hop radius for the OVERVIEW: pick how many undirected hops of neighbours to
    // brighten around this node. Selects the node and remembers the radius for plain clicks.
    // status: code-graph
    menu = menu
        .section()
        .action_with(Action::new("Highlight 1 hop", NodeAction::FocusHops(1)))
        .action_with(Action::new("Highlight 2 hops", NodeAction::FocusHops(2)))
        .action_with(Action::new("Highlight 3 hops", NodeAction::FocusHops(3)));
    if !specs.is_empty() {
        menu = menu.section();
        for s in specs {
            menu = menu.action(format!("Select spec {s}"), NodeAction::SelectSpec(s.clone()));
        }
    }
    menu
}

/// A SPEC node's right-click menu: open its defining doc (read-only preview tab) + copy the slug.
/// status: code-graph-open-diff-from-node
pub fn spec_node_menu(slug: &str) -> Menu<NodeAction> {
    let copy = slug.to_owned();
    Menu::new().action("Open spec doc", NodeAction::OpenSource).custom(move |ui| {
        if ui.button("Copy slug").clicked() {
            ui.ctx().copy_text(copy.clone());
            ui.close();
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::path::Path;

    use egui_workbench::menu::{Enabled, Entry};
    use hiker_git::repo::ChangeStatus;

    use super::{
        gov_color, node_menu, rows_to_repo, Changes, DiffVerb, GovState, NodeAction,
        DIFF_BODY_SAME_DIM,
    };

    fn enabled_of<A>(entry: &Entry<A>) -> &Enabled {
        match entry {
            Entry::Action { enabled, .. } => enabled,
            _ => panic!("expected an Action entry"),
        }
    }

    fn action_of<A: Clone>(entry: &Entry<A>, expect_label: &str) -> A {
        match entry {
            Entry::Action { label, action, .. } => {
                assert_eq!(label, expect_label, "entry label");
                action.clone()
            }
            _ => panic!("expected an Action entry for {expect_label}"),
        }
    }

    /// Vault-relative diff rows map onto repo-relative paths; rows outside the repo root drop.
    #[test]
    fn rows_to_repo_strips_the_repo_prefix() {
        let rows = vec![
            ("code/hiker/src/lib.rs".to_string(), ChangeStatus::Modified),
            ("notes/daily.md".to_string(), ChangeStatus::Added),
        ];
        let map = rows_to_repo(rows, Path::new("/vault"), Path::new("/vault/code/hiker"));
        assert_eq!(map.get("src/lib.rs"), Some(&ChangeStatus::Modified));
        assert!(!map.contains_key("notes/daily.md"), "out-of-repo rows dropped");
        assert_eq!(map.len(), 1);
    }

    /// The change ring (`code-graph-diff-symbol-level`, now a direct node mark): in a refined file a
    /// body-changed symbol rings full and a body-identical one rings dim; an UNREFINED changed file
    /// rings full for every node; an unchanged file has no ring; and `touches` follows the same
    /// refinement (the "only changed" lens predicate).
    #[test]
    fn change_ring_and_touches_follow_refinement() {
        let changes = Changes {
            files: HashMap::from([
                ("src/a.rs".to_string(), ChangeStatus::Modified),
                ("src/b.rs".to_string(), ChangeStatus::Modified),
            ]),
            sym: HashMap::from([("src/a.rs".to_string(), HashSet::from(["hot".to_string()]))]),
        };
        let full = crate::panels::git_diff::status_glyph(ChangeStatus::Modified).1;
        // Refined file: body-changed → full ring, body-identical → dim ring.
        assert_eq!(changes.ring("src/a.rs", "hot").unwrap().color, full);
        assert_eq!(
            changes.ring("src/a.rs", "cold").unwrap().color,
            full.gamma_multiply(DIFF_BODY_SAME_DIM),
            "HEAD-identical body → dimmed ring"
        );
        // Unrefined changed file → full ring for any node; unchanged file → no ring.
        assert_eq!(changes.ring("src/b.rs", "any").unwrap().color, full);
        assert!(changes.ring("src/c.rs", "any").is_none(), "unchanged file → no ring");
        // `touches`: refined file only the changed body; unrefined file any node; unchanged none.
        assert!(changes.touches("src/a.rs", "hot"));
        assert!(!changes.touches("src/a.rs", "cold"), "refined: only the changed body");
        assert!(changes.touches("src/b.rs", "any"), "unrefined: whole file counts");
        assert!(!changes.touches("src/c.rs", "any"));
    }

    /// Every governance state has a distinct drift color, and ungoverned is the muted mass.
    #[test]
    fn governance_palette_is_distinct() {
        let colors = [
            gov_color(GovState::Ok),
            gov_color(GovState::Drifted),
            gov_color(GovState::Missing),
            gov_color(GovState::Ungoverned),
        ];
        for (i, a) in colors.iter().enumerate() {
            for b in &colors[i + 1..] {
                assert_ne!(a, b, "states must be visually distinct");
            }
        }
    }

    /// Menu composition: Open source, Open diff (greyed without git), the Highlight-hops section,
    /// then the Select-spec section per governing spec.
    #[test]
    fn node_menu_offers_open_diff_highlight_and_select_spec() {
        let specs = vec!["spec-a".to_string(), "spec-b".to_string()];
        let menu = node_menu("moniker", DiffVerb::Ready, &specs);
        let sections = menu.sections();
        assert_eq!(sections.len(), 3, "verbs + highlight-hops + select-spec sections");
        assert_eq!(action_of(&sections[0][0], "Open source"), NodeAction::OpenSource);
        assert_eq!(action_of(&sections[0][1], "Open diff vs HEAD"), NodeAction::OpenDiff);
        assert!(enabled_of(&sections[0][1]).is_enabled(), "git Ready → clickable");
        // The highlight-hops section: 1/2/3-hop radius entries.
        assert_eq!(sections[1].len(), 3, "1/2/3-hop highlight entries");
        assert_eq!(action_of(&sections[1][0], "Highlight 1 hop"), NodeAction::FocusHops(1));
        assert_eq!(action_of(&sections[1][1], "Highlight 2 hops"), NodeAction::FocusHops(2));
        assert_eq!(action_of(&sections[1][2], "Highlight 3 hops"), NodeAction::FocusHops(3));
        assert_eq!(
            action_of(&sections[2][0], "Select spec spec-a"),
            NodeAction::SelectSpec("spec-a".into())
        );
        assert_eq!(sections[2].len(), 2, "one entry per governing spec");

        // No git → Open diff greyed; no governing specs → no select section (but highlight stays).
        let menu = node_menu("moniker", DiffVerb::NoGit, &[]);
        let sections = menu.sections();
        assert_eq!(sections.len(), 2, "verbs + highlight; no governing specs → no select section");
        assert!(!enabled_of(&sections[0][1]).is_enabled(), "NoGit → greyed");
    }
}
