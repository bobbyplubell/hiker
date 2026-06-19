//! Shared focus-navigation scaffolding for the graph panels
//! (`graph-nav-extract`): the depth-bounded neighbourhood BFS, the
//! Overview / 1–3-hops scope dial, the global Back/Forward toolbar controls,
//! the Esc middle-rung gate, and the scope persistence round-trip. The vault
//! and code graph panels both drive these over the shared [`Scope`] type;
//! each panel keeps its own policy (what counts as an edge, what's drawn,
//! how a drill is recorded).

use eframe::egui;

use crate::tab::Scope;
use hiker_theme as theme;

/// Membership mask of the depth-bounded undirected neighbourhood of `focus`:
/// BFS over the symmetric adjacency built from `edges` (index pairs into a
/// `node_count`-sized node set). Generic over the caller's edge storage — the
/// code graph feeds its full structural adjacency, the vault graph the typed
/// edges surviving its kind toggles. An out-of-range `focus` yields an empty
/// mask. status: graph-nav-extract
pub(crate) fn hop_mask(
    node_count: usize,
    edges: impl Iterator<Item = (usize, usize)>,
    focus: usize,
    depth: usize,
) -> Vec<bool> {
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); node_count];
    for (a, b) in edges {
        if a < node_count && b < node_count {
            adj[a].push(b);
            adj[b].push(a);
        }
    }
    let mut seen = vec![false; node_count];
    if focus >= node_count {
        return seen;
    }
    let mut q = std::collections::VecDeque::from([(focus, 0usize)]);
    seen[focus] = true;
    while let Some((n, d)) = q.pop_front() {
        if d == depth {
            continue;
        }
        for &m in &adj[n] {
            if !seen[m] {
                seen[m] = true;
                q.push_back((m, d + 1));
            }
        }
    }
    seen
}

/// String discriminant of a [`Scope`] for the persisted view-state records
/// (the enum lives in the app crate, not core). status: graph-view-state-persist
pub(crate) fn scope_persist_str(scope: Scope) -> String {
    match scope {
        Scope::Overview => "overview".to_string(),
        Scope::Hops(n) => format!("hops:{n}"),
    }
}

/// Parse a persisted scope discriminant; junk / out-of-range hop counts fall
/// back to the overview. status: graph-view-state-persist
pub(crate) fn scope_from_persist_str(s: &str) -> Scope {
    match s.strip_prefix("hops:").and_then(|n| n.parse::<u8>().ok()) {
        Some(n) if (1..=3).contains(&n) => Scope::Hops(n),
        _ => Scope::Overview,
    }
}

/// Render the toolbar Back/Forward BUTTONS (+ mouse Extra1/Extra2) and report
/// the requested GLOBAL nav delta (`-1` back, `+1` forward). Alt+←/→ and
/// Mod-[/] ride the global keybind path, not here. The buttons mirror the
/// global stack's `can_back()`/`can_forward()`; the caller routes the delta
/// through `nav_go` → `navigate_to`. status: graph-nav-extract
pub(crate) fn nav_controls(ui: &mut egui::Ui, can_back: bool, can_fwd: bool) -> Option<i32> {
    let (mouse_back, mouse_fwd) = ui.input(|i| {
        (
            i.pointer.button_clicked(egui::PointerButton::Extra1),
            i.pointer.button_clicked(egui::PointerButton::Extra2),
        )
    });
    let back = ui
        .add_enabled(can_back, egui::Button::new("⟵").small())
        .on_hover_text("Back (Alt+←)")
        .clicked();
    let fwd = ui
        .add_enabled(can_fwd, egui::Button::new("⟶").small())
        .on_hover_text("Forward (Alt+→)")
        .clicked();
    if can_back && (back || mouse_back) {
        Some(-1)
    } else if can_fwd && (fwd || mouse_fwd) {
        Some(1)
    } else {
        None
    }
}

/// The shared Scope dial: Overview, or the anchor's 1/2/3-hop neighbourhood.
/// The hop positions need an anchor — disabled (with a hint) until
/// `anchor_label` exists; while in hops scope the anchor's display name tags
/// the dial. Mutates `scope` in place; the caller's change detection drives
/// the rebuild. status: graph-nav-extract
pub(crate) fn scope_dial(ui: &mut egui::Ui, scope: &mut Scope, anchor_label: Option<&str>) {
    ui.label(egui::RichText::new("Scope:").small().color(theme::muted()));
    if ui.selectable_label(*scope == Scope::Overview, "Overview").clicked() {
        *scope = Scope::Overview;
    }
    for d in [1u8, 2, 3] {
        let resp = ui
            .add_enabled_ui(anchor_label.is_some(), |ui| {
                ui.selectable_label(*scope == Scope::Hops(d), d.to_string())
            })
            .inner;
        if anchor_label.is_none() {
            resp.clone().on_hover_text("Select a node first");
        }
        if resp.clicked() {
            *scope = Scope::Hops(d);
        }
    }
    if matches!(scope, Scope::Hops(_)) {
        let name = anchor_label.unwrap_or("?");
        ui.label(egui::RichText::new(format!("@ {name}")).small().color(theme::accent()));
    }
}

/// The Esc ladder's middle rung gate (`interaction.md` [keyboard-esc-ladder]):
/// true when this frame's Esc should pop a hops focus back to the overview —
/// scope is focused, Esc was pressed, no popup consumed it first
/// (`taken_by_popup`: an open find popup / latched node menu closes on Esc),
/// and no text field holds focus (Esc there means "drop focus", egui's
/// default). status: graph-nav-extract
pub(crate) fn esc_pops_focus(ui: &egui::Ui, scope: Scope, taken_by_popup: bool) -> bool {
    !taken_by_popup
        && matches!(scope, Scope::Hops(_))
        && ui.input(|i| i.key_pressed(egui::Key::Escape))
        && ui.ctx().memory(|m| m.focused().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BFS over a line graph 0—1—2—3: depth bounds the reach, symmetrically.
    #[test]
    fn hop_mask_bounds_the_neighbourhood_by_depth() {
        let edges = [(0usize, 1usize), (1, 2), (2, 3)];
        let one = hop_mask(4, edges.iter().copied(), 1, 1);
        assert_eq!(one, vec![true, true, true, false]);
        let two = hop_mask(4, edges.iter().copied(), 0, 2);
        assert_eq!(two, vec![true, true, true, false]);
        let all = hop_mask(4, edges.iter().copied(), 0, 3);
        assert!(all.iter().all(|&v| v));
    }

    /// Direction never matters (the neighbourhood is undirected) and an
    /// out-of-range focus / edge index degrades to an empty / partial mask
    /// instead of panicking.
    #[test]
    fn hop_mask_is_undirected_and_bounds_checked() {
        let edges = [(2usize, 0usize)];
        let mask = hop_mask(3, edges.iter().copied(), 0, 1);
        assert_eq!(mask, vec![true, false, true], "reverse edge still reachable");
        assert!(hop_mask(3, [(9usize, 0usize)].iter().copied(), 0, 1)[0]);
        assert!(hop_mask(2, [].iter().copied(), 5, 1).iter().all(|&v| !v));
    }

    /// Scope persists as a string discriminant and round-trips; junk falls
    /// back to overview.
    #[test]
    fn scope_persist_round_trip() {
        for s in [Scope::Overview, Scope::Hops(1), Scope::Hops(3)] {
            assert_eq!(scope_from_persist_str(&scope_persist_str(s)), s);
        }
        assert_eq!(scope_from_persist_str("hops:9"), Scope::Overview, "out-of-range clamps");
        assert_eq!(scope_from_persist_str("objects"), Scope::Overview, "old level strings");
        assert_eq!(scope_from_persist_str(""), Scope::Overview, "pre-feature empty");
    }
}
