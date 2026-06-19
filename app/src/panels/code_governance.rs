//! Overlay layer for the code graph panel: the spec-governance and git-diff fill
//! overlays (`code-graph-governance-overlay`, `code-graph-diff-coloring` + its
//! symbol-grain refinement `code-graph-diff-symbol-level`), spec
//! lighting (`code-graph-spec-lighting`), the planned/partial status badge
//! (`code-graph-status-badge`), and the node context menu with its "Open diff"
//! verb (`code-graph-open-diff-from-node`). The panel (`code_graph.rs`) owns the
//! view + render wiring; this module owns the overlay *data and policy* — which
//! mode is active, what each node's fill/badge is, which verbs its menu offers —
//! so the boundary is "what color/verb does this node get", not a length split.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use eframe::egui;
use egui_workbench::menu::{Action, Enabled, Menu};

use hiker_code::governance::Governance;
use hiker_code::{GovState, ScipAdapter};
use hiker_git::repo::ChangeStatus;
use hiker_theme as theme;
use spec_engine::SourceId;

/// What the node fills encode. The modes are **mutually exclusive** — all three
/// compete for the one fill channel (kind → shape stays constant across modes),
/// and layering two color encodings on one fill is unreadable; the toolbar dial
/// makes the active encoding explicit instead. status: code-graph-governance-overlay
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayMode {
    /// Fills color by entity kind (the original encoding).
    #[default]
    Kind,
    /// Fills color by spec-governance state (ok / drifted / missing / ungoverned).
    Governance,
    /// Fills color by HEAD-vs-worktree change status of the node's file.
    Diff,
}

/// Per-view overlay state: the mode dial plus the lazily-loaded data behind each
/// mode. Governance (a full drift pass over `links.json`) loads once on the first
/// switch; the diff map reloads on every switch into Diff so it tracks the
/// working tree.
#[derive(Default)]
pub struct Overlay {
    pub mode: OverlayMode,
    /// Drift rollup + doc statuses; `None` until first needed, `Err` = load failure.
    gov: Option<Result<Governance, String>>,
    /// Repo-relative changed files (HEAD vs worktree); `None` until Diff mode.
    diff: Option<Result<HashMap<String, ChangeStatus>, String>>,
    /// Symbol-grain refinement of `diff` (`code-graph-diff-symbol-level`): per *refined*
    /// changed file, the monikers whose definition body actually differs vs HEAD. A file
    /// with no entry couldn't be refined (added/deleted/renamed path, no HEAD text, no AST
    /// grammar, parse failure) and keeps the louder file-grain color for every node.
    sym_diff: HashMap<String, HashSet<String>>,
    /// The spec lit up in governance mode (`None` = no lighting).
    pub spec: Option<String>,
    /// Monikers the lit spec covers: targets + 1-hop blast radius.
    lit: Option<HashSet<String>>,
    /// Cached `links.json`-exists check (a per-frame stat would be waste).
    links_present: Option<bool>,
}

/// Governance fill palette: calm green / amber / red for the linked states, and
/// the muted gray that turns the ungoverned share of the graph into a literally
/// visible mass.
const GOV_OK: egui::Color32 = egui::Color32::from_rgb(0x3d, 0x9a, 0x5f);
const GOV_DRIFTED: egui::Color32 = egui::Color32::from_rgb(0xd9, 0x9a, 0x2b);
const GOV_MISSING: egui::Color32 = egui::Color32::from_rgb(0xc8, 0x45, 0x4f);
/// The muted "not in this overlay's story" fill: ungoverned nodes in governance
/// mode, unchanged files in diff mode.
const MUTED_MASS: egui::Color32 = egui::Color32::from_rgb(0x55, 0x59, 0x60);
/// Status-badge dot: violet — distinct from every governance/diff fill so the
/// badge reads as a mark, not a state.
const BADGE: egui::Color32 = egui::Color32::from_rgb(0xb1, 0x7f, 0xe8);
/// Open-bugs badge dot (the top-LEFT twin): hot coral, distinct from the violet
/// status badge and from every governance fill — two marks, two channels.
const BUG_BADGE: egui::Color32 = egui::Color32::from_rgb(0xf0, 0x62, 0x4d);
/// Diff-mode dim for "the file churned around it" (`code-graph-diff-symbol-level`):
/// a body-unchanged symbol in a changed file keeps its file's status color at this
/// reduced strength — still in the diff story (≠ [`MUTED_MASS`]), but visibly
/// quieter than a body-changed node at full color.
const DIFF_BODY_SAME_DIM: f32 = 0.45;

/// Fill for a folded governance state.
pub const fn gov_color(state: GovState) -> egui::Color32 {
    match state {
        GovState::Ok => GOV_OK,
        GovState::Drifted => GOV_DRIFTED,
        GovState::Missing => GOV_MISSING,
        GovState::Ungoverned => MUTED_MASS,
    }
}

/// One-word label for a folded governance state (detail line / counts).
pub const fn gov_label(state: GovState) -> &'static str {
    match state {
        GovState::Ok => "ok",
        GovState::Drifted => "drifted",
        GovState::Missing => "missing",
        GovState::Ungoverned => "ungoverned",
    }
}

impl Overlay {
    /// Whether the repo carries a `links.json` drift baseline (cached stat) —
    /// gates the governance mode toggle.
    fn links_present(&mut self, repo_root: &Path) -> bool {
        *self.links_present.get_or_insert_with(|| repo_root.join("links.json").exists())
    }

    /// Load the governance rollup once (drift-checks every linked body — a real
    /// one-time cost on big stores, paid on the first switch into the mode).
    /// `pub(crate)` so the vault graph's spec-jump landing
    /// (`vault-graph-spec-drift-badge`) can light a spec on a freshly-built
    /// view through `code_graph::light_spec`.
    pub(crate) fn ensure_governance(&mut self, adapter: &ScipAdapter, src: &SourceId) {
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

    /// The data-load error behind the active mode, if any (rendered by the toolbar).
    fn load_error(&self) -> Option<&str> {
        let res = match self.mode {
            OverlayMode::Kind => return None,
            OverlayMode::Governance => self.gov.as_ref().map(|g| g.as_ref().err()),
            OverlayMode::Diff => self.diff.as_ref().map(|d| d.as_ref().err()),
        };
        res.flatten().map(String::as_str)
    }

    /// The change status of a repo-relative file, when the diff map is loaded.
    fn diff_status(&self, file: &str) -> Option<ChangeStatus> {
        self.diff
            .as_ref()
            .and_then(|d| d.as_ref().ok())
            .and_then(|m| m.get(file).copied())
    }

    /// Light `spec` (or clear with `None`): records the selection and computes the
    /// lit moniker set (targets + blast radius). The caller pulses the engine with
    /// the lit nodes' display indices. status: code-graph-spec-lighting
    pub fn light(&mut self, spec: Option<String>, adapter: &ScipAdapter, src: &SourceId) {
        self.lit = match (&spec, self.governance()) {
            (Some(s), Some(gov)) => Some(gov.lighting(s, adapter, src)),
            _ => None,
        };
        self.spec = spec;
    }

    /// Whether `id` is in the lit spec's set (true when no lighting is active —
    /// nothing is dimmed then).
    fn lit_or_no_lighting(&self, id: &str) -> bool {
        self.lit.as_ref().is_none_or(|l| l.contains(id))
    }

    /// The lit monikers, for mapping onto display indices.
    pub const fn lit_ids(&self) -> Option<&HashSet<String>> {
        self.lit.as_ref()
    }

    /// The fill for a node under the active overlay: `base` (the kind color) in
    /// Kind mode, the governance/diff palette otherwise — with everything outside
    /// a lit spec's set dimmed toward the background so the spec's footprint pops.
    pub fn node_fill(&self, base: egui::Color32, id: &str, file: &str) -> egui::Color32 {
        let fill = match self.mode {
            OverlayMode::Kind => base,
            OverlayMode::Governance => {
                gov_color(self.governance().map_or(GovState::Ungoverned, |g| g.state_of(id)))
            }
            OverlayMode::Diff => match self.diff_status(file) {
                Some(status) => {
                    let full = super::git_diff::status_glyph(status).1;
                    // Body fingerprint HEAD-identical → the file churned *around* this
                    // symbol: dim the status color. Only files the refinement could
                    // PROVE something about have a `sym_diff` entry — everything else
                    // over-flags at full file-grain color, never silently dims.
                    // status: code-graph-diff-symbol-level
                    if self.sym_diff.get(file).is_some_and(|changed| !changed.contains(id)) {
                        full.gamma_multiply(DIFF_BODY_SAME_DIM)
                    } else {
                        full
                    }
                }
                None => MUTED_MASS,
            },
        };
        if self.mode == OverlayMode::Governance && !self.lit_or_no_lighting(id) {
            fill.gamma_multiply(0.25)
        } else {
            fill
        }
    }

    /// The badge dot for a node: governance mode only, on nodes any of whose
    /// governing specs is `status:: planned`/`partial`. status: code-graph-status-badge
    pub fn node_badge(&self, id: &str) -> Option<egui::Color32> {
        (self.mode == OverlayMode::Governance
            && self.governance().is_some_and(|g| g.flagged(id)))
        .then_some(BADGE)
    }

    /// The open-bugs badge dot (top-left shoulder): governance mode only, on
    /// nodes with a `manifests-in` edge from a non-struck bug row.
    /// status: code-graph-bug-badge
    pub fn node_bug_badge(&self, id: &str) -> Option<egui::Color32> {
        (self.mode == OverlayMode::Governance
            && self.governance().is_some_and(|g| !g.open_bugs_of(id).is_empty()))
        .then_some(BUG_BADGE)
    }

    /// One detail-line fragment for the selected node's governance: state +
    /// governing specs (with any flagged status) + open bugs, once the rollup is
    /// loaded. Bug-only nodes (governed solely by bug edges) skip the empty spec
    /// list rather than printing a dangling separator.
    pub fn detail_fragment(&self, id: &str) -> Option<String> {
        let gov = self.governance()?;
        let state = gov.state_of(id);
        let bugs = gov.open_bugs_of(id);
        let mut parts = Vec::new();
        if state == GovState::Ungoverned {
            parts.push("spec: ungoverned".to_string());
        } else {
            let specs: Vec<String> = gov
                .specs_of(id)
                .iter()
                .map(|s| match gov.status_of(s) {
                    Some(st) if hiker_code::governance::status_flagged(st) => format!("{s} ({st})"),
                    _ => s.clone(),
                })
                .collect();
            parts.push(if specs.is_empty() {
                format!("spec: {}", gov_label(state))
            } else {
                format!("spec: {} · {}", gov_label(state), specs.join(", "))
            });
        }
        if !bugs.is_empty() {
            // status: code-graph-bug-badge
            let plural = if bugs.len() == 1 { "" } else { "s" };
            parts.push(format!("{} open bug{plural}: {}", bugs.len(), bugs.join(", ")));
        }
        Some(parts.join("  ·  "))
    }
}

/// Map the vault git engine's changed rows (vault-relative) into repo-relative
/// paths under `repo_root` — rows outside the repo are dropped. Pure; the repo
/// root was resolved against the vault root at bind, so prefix-stripping is
/// exact. status: code-graph-diff-coloring
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

/// Symbol-grain refinement pass (`code-graph-diff-symbol-level`): for each `Modified` file
/// in the diff map, fetch its HEAD text (`show_at` — `git show HEAD:<path>`) and ask the
/// adapter which definition bodies actually differ (name-anchored AST fingerprints — the
/// drift machinery vs HEAD instead of vs baseline). Only files the refinement can *prove*
/// something about get an entry; everything else — added / deleted / renamed paths (no
/// comparable HEAD body), no HEAD text, no AST grammar, an out-of-vault repo root — keeps
/// the louder file-grain color: failures over-flag, never silently dim.
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

/// Governance counts over a node-id iterator: `[ok, drifted, missing, ungoverned]`
/// — the summary line's "the ungoverned mass, numerically".
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

/// Outcome of the toolbar's overlay section.
#[derive(Default)]
pub struct OverlayResult {
    /// Node fills changed (mode switch / lighting change / diff reload): the
    /// panel invalidates the engine's GPU paint cache (fills are baked into the
    /// cached affine batch — see `State::invalidate_paint_cache`).
    pub recolor: bool,
    /// The lit spec changed: the panel pulses the lit nodes through the fluid
    /// highlight.
    pub pulse: bool,
}

/// The toolbar's overlay section: the `Kind | Spec | Diff` mode dial (each data
/// mode disabled with a reason when its data source is absent), the spec-lighting
/// dropdown in governance mode, and any data-load error. Loads governance lazily
/// on the first switch; reloads the diff map on every switch into Diff so it
/// tracks the working tree.
pub fn toolbar_section(
    ui: &mut egui::Ui,
    overlay: &mut Overlay,
    adapter: &ScipAdapter,
    src: &SourceId,
    git: Option<&crate::git_sync::GitSyncEngine>,
    vault_root: &Path,
) -> OverlayResult {
    let mut result = OverlayResult::default();
    ui.label(egui::RichText::new("Overlay:").small().color(theme::muted()));
    if ui.selectable_label(overlay.mode == OverlayMode::Kind, "Kind").clicked()
        && overlay.mode != OverlayMode::Kind
    {
        overlay.mode = OverlayMode::Kind;
        result.recolor = true;
    }

    let links = overlay.links_present(adapter.repo_root());
    let spec_resp = ui
        .add_enabled(links, egui::Button::selectable(overlay.mode == OverlayMode::Governance, "Spec"))
        .on_hover_text("Color nodes by spec governance: ok / drifted / missing / ungoverned");
    if !links {
        spec_resp.clone().on_hover_text("No links.json at the repo root");
    }
    if spec_resp.clicked() && overlay.mode != OverlayMode::Governance {
        overlay.mode = OverlayMode::Governance;
        overlay.ensure_governance(adapter, src);
        result.recolor = true;
    }

    let diff_resp = ui
        .add_enabled(git.is_some(), egui::Button::selectable(overlay.mode == OverlayMode::Diff, "Diff"))
        .on_hover_text(
            "Color nodes by change vs HEAD: full = symbol body changed, \
             dim = file churned around an unchanged body",
        );
    if git.is_none() {
        diff_resp.clone().on_hover_text("Git isn't enabled for this vault");
    }
    if diff_resp.clicked()
        && overlay.mode != OverlayMode::Diff
        && let Some(git) = git
    {
        overlay.mode = OverlayMode::Diff;
        let map = git
            .diff_paths("HEAD", None)
            .map(|rows| rows_to_repo(rows, vault_root, adapter.repo_root()));
        overlay.sym_diff = match &map {
            Ok(m) => refine_symbol_diff(m, adapter, git, vault_root),
            Err(_) => HashMap::new(),
        };
        overlay.diff = Some(map);
        result.recolor = true;
    }

    if overlay.mode == OverlayMode::Governance
        && overlay.governance().is_some()
        && spec_dropdown(ui, overlay, adapter, src)
    {
        result.recolor = true;
        result.pulse = true;
    }
    if let Some(err) = overlay.load_error() {
        ui.colored_label(egui::Color32::RED, err);
    }
    result
}

/// The spec-lighting dropdown: every lightable spec (one with `implements`/
/// `touches` targets), plus "(none)" to clear. Returns whether the selection
/// changed.
fn spec_dropdown(
    ui: &mut egui::Ui,
    overlay: &mut Overlay,
    adapter: &ScipAdapter,
    src: &SourceId,
) -> bool {
    let mut pick: Option<Option<String>> = None;
    egui::ComboBox::from_id_salt("code-gov-spec")
        .selected_text(overlay.spec.as_deref().unwrap_or("(light a spec)"))
        .width(180.0)
        .show_ui(ui, |ui| {
            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                if ui.selectable_label(overlay.spec.is_none(), "(none)").clicked() {
                    pick = Some(None);
                }
                let specs: Vec<String> = overlay
                    .governance()
                    .map(|g| g.specs().cloned().collect())
                    .unwrap_or_default();
                for s in specs {
                    if ui.selectable_label(overlay.spec.as_deref() == Some(&s), &s).clicked() {
                        pick = Some(Some(s));
                    }
                }
            });
        });
    match pick {
        Some(spec) if spec != overlay.spec => {
            overlay.light(spec, adapter, src);
            true
        }
        _ => false,
    }
}

/// A verb picked from the node context menu, applied by the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeAction {
    /// Open the node's source file read-only (the old direct right-click verb).
    OpenSource,
    /// Open the node's file with the `GitRef` diff overlay vs HEAD.
    OpenDiff,
    /// Light this governing spec (same effect as the toolbar dropdown).
    LightSpec(String),
}

/// Why the "Open diff" verb is or isn't available, carried into its enabled state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffVerb {
    /// Diff overlay active and the node's file changed vs HEAD.
    Ready,
    /// Diff overlay active but the file is unchanged.
    Unchanged,
    /// Diff overlay not active (the changed-file map isn't loaded).
    OverlayOff,
    /// No git engine for this vault.
    NoGit,
}

impl Overlay {
    /// The "Open diff" availability for a node's `file` — feeds [`node_menu`].
    pub fn diff_verb(&self, file: &str, has_git: bool) -> DiffVerb {
        if !has_git {
            DiffVerb::NoGit
        } else if self.mode != OverlayMode::Diff || self.diff.is_none() {
            DiffVerb::OverlayOff
        } else if self.diff_status(file).is_some() {
            DiffVerb::Ready
        } else {
            DiffVerb::Unchanged
        }
    }
}

/// The node's right-click menu (`interaction.md` [rightclick-menu-always]:
/// right-click is a MENU, never a direct action — this replaces the old direct
/// "open source" binding): Open source / Open diff (greyed with the reason when
/// unavailable) / Copy symbol, plus a "Light spec" section when governance knows
/// the node's specs. status: code-graph-open-diff-from-node
pub fn node_menu(moniker: &str, diff: DiffVerb, specs: &[String]) -> Menu<NodeAction> {
    let open_diff = match diff {
        DiffVerb::Ready => Action::new("Open diff vs HEAD", NodeAction::OpenDiff),
        DiffVerb::Unchanged => Action::new("Open diff vs HEAD", NodeAction::OpenDiff)
            .enabled(Enabled::No("file unchanged vs HEAD".into())),
        DiffVerb::OverlayOff => Action::new("Open diff vs HEAD", NodeAction::OpenDiff)
            .enabled(Enabled::No("switch the overlay to Diff first".into())),
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
    if !specs.is_empty() {
        menu = menu.section();
        for s in specs {
            menu = menu.action(format!("Light spec {s}"), NodeAction::LightSpec(s.clone()));
        }
    }
    menu
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use egui_workbench::menu::{Enabled, Entry};
    use hiker_git::repo::ChangeStatus;

    use super::{gov_color, node_menu, rows_to_repo, DiffVerb, GovState, NodeAction};

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

    /// Vault-relative diff rows map onto repo-relative paths; rows outside the
    /// repo root are dropped.
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

        // Vault root == repo root (the whole-repo-vault case): identity mapping.
        let rows = vec![("src/lib.rs".to_string(), ChangeStatus::Renamed)];
        let map = rows_to_repo(rows, Path::new("/repo"), Path::new("/repo"));
        assert_eq!(map.get("src/lib.rs"), Some(&ChangeStatus::Renamed));
    }

    /// Symbol-grain diff fill (`code-graph-diff-symbol-level`): in a refined changed file a
    /// body-changed symbol keeps the full status color and a body-identical one dims (the
    /// file churned around it); an UNREFINED changed file keeps full color for every node
    /// (failures over-flag, never silently dim); unchanged files stay the muted mass.
    #[test]
    fn diff_fill_dims_only_proven_unchanged_bodies() {
        use std::collections::{HashMap, HashSet};

        use super::{Overlay, OverlayMode, DIFF_BODY_SAME_DIM, MUTED_MASS};
        let overlay = Overlay {
            mode: OverlayMode::Diff,
            diff: Some(Ok(HashMap::from([
                ("src/a.rs".to_string(), ChangeStatus::Modified),
                ("src/b.rs".to_string(), ChangeStatus::Modified),
            ]))),
            sym_diff: HashMap::from([(
                "src/a.rs".to_string(),
                HashSet::from(["hot".to_string()]),
            )]),
            ..Default::default()
        };
        let base = eframe::egui::Color32::WHITE;
        let full = crate::panels::git_diff::status_glyph(ChangeStatus::Modified).1;
        assert_eq!(overlay.node_fill(base, "hot", "src/a.rs"), full, "changed body → full");
        assert_eq!(
            overlay.node_fill(base, "cold", "src/a.rs"),
            full.gamma_multiply(DIFF_BODY_SAME_DIM),
            "HEAD-identical body → dimmed status color"
        );
        assert_eq!(
            overlay.node_fill(base, "any", "src/b.rs"),
            full,
            "unrefined changed file stays at file grain"
        );
        assert_eq!(
            overlay.node_fill(base, "any", "src/c.rs"),
            MUTED_MASS,
            "unchanged file stays the muted mass"
        );
    }

    /// Every governance state has a distinct fill, and ungoverned shares the
    /// muted-mass gray with diff-unchanged (the "not this overlay's story" tone).
    /// The two badge marks (status / open-bugs) are distinct from each other and
    /// from every fill — a mark must never read as a state.
    #[test]
    fn governance_palette_is_distinct() {
        let colors = [
            gov_color(GovState::Ok),
            gov_color(GovState::Drifted),
            gov_color(GovState::Missing),
            gov_color(GovState::Ungoverned),
            super::BADGE,
            super::BUG_BADGE,
        ];
        for (i, a) in colors.iter().enumerate() {
            for b in &colors[i + 1..] {
                assert_ne!(a, b, "states and marks must be visually distinct");
            }
        }
    }

    /// Menu composition: Open source, then Open diff (enabled only when Ready,
    /// otherwise greyed with the reason), then the Light-spec section per
    /// governing spec.
    #[test]
    fn node_menu_offers_open_diff_and_light_spec() {
        let specs = vec!["spec-a".to_string(), "spec-b".to_string()];
        let menu = node_menu("moniker", DiffVerb::Ready, &specs);
        let sections = menu.sections();
        assert_eq!(sections.len(), 2, "verbs section + light-spec section");
        assert_eq!(action_of(&sections[0][0], "Open source"), NodeAction::OpenSource);
        assert_eq!(action_of(&sections[0][1], "Open diff vs HEAD"), NodeAction::OpenDiff);
        assert!(enabled_of(&sections[0][1]).is_enabled(), "Ready → clickable");
        assert_eq!(sections[0].len(), 3, "Open source + Open diff + Copy symbol");
        assert_eq!(
            action_of(&sections[1][0], "Light spec spec-a"),
            NodeAction::LightSpec("spec-a".into())
        );
        assert_eq!(sections[1].len(), 2, "one entry per governing spec");

        // Unavailable diff verbs grey out (menu stays complete, per the grammar).
        for verb in [DiffVerb::Unchanged, DiffVerb::OverlayOff, DiffVerb::NoGit] {
            let menu = node_menu("moniker", verb, &[]);
            let sections = menu.sections();
            assert_eq!(sections.len(), 1, "no governing specs → no light section");
            assert!(!enabled_of(&sections[0][1]).is_enabled(), "{verb:?} → greyed");
        }
    }
}
