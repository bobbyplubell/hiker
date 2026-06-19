//! Headless diff-overlay repro / snapshot tool.
//!
//! Builds the *real* layered-doc accepted / working / pending buffers (via
//! `hiker_core::editing::LayeredDoc`) for a matrix of user / agent / sync scenarios,
//! then for each one:
//!   1. Prints a text report of the agent-overlay geometry, using the SAME
//!      functions the app's `attach_agent_hunk_widgets` uses:
//!        - `editor_diff::overlay`  — hunk byte ranges + action-row anchor/side
//!        - `editor_diff::conflict` — conflict (user-vs-agent) detection
//!        - `op_writes::ops_in_hunk` — which pending ops a hunk covers
//!        - `LayeredDoc::is_pending_drifted` — whether Accept should be disabled
//!   2. Renders the editor (working buffer + `proposal_decorations` green/strike
//!      + the per-hunk action rows, with Accept greyed on drift and verbs
//!      skipped when no op covers the hunk) to a PNG via `egui_kittest`.
//!
//! The diff is `diff(materialize_working, materialize_review(session))` — the
//! editable buffer vs working+pending — exactly what the app's `agent_overlay`
//! diffs. So a placement / state bug here is one in the app.
//!
//! Usage:  cargo run -p diff-overlay-snapshot
//! Output: target/diff-overlay-<scenario>.png + a report on stdout.

use anyhow::Result;
use editor_core::decoration::BlockSide;
use editor_core::diff::HunkKind;
use editor_core::state::Editor;
use editor_diff::conflict;
use editor_diff::overlay as ov;
use hiker_core::ops::op_writes;
use hiker_core::editing::shapes::Author;
use hiker_core::editing::{EditSpec, LayeredDoc, ProducerCtx};

// Screenshot-only imports (see Cargo `[features].screenshot`).
#[cfg(feature = "screenshot")]
use std::ops::Range;
#[cfg(feature = "screenshot")]
use std::panic::AssertUnwindSafe;
#[cfg(feature = "screenshot")]
use std::path::Path;
#[cfg(feature = "screenshot")]
use editor_core::decoration::{
    ActionButton, ActionButtonStyle, ActionTone, BlockDeco, BlockKind, Decoration, Set,
};
#[cfg(feature = "screenshot")]
use editor_core::rangeset::RangeSet;
#[cfg(feature = "screenshot")]
use editor_diff::view::proposal_decorations;
#[cfg(feature = "screenshot")]
use editor_egui::widget::{PaintCache, Widget as EditorWidget};
#[cfg(feature = "screenshot")]
use editor_view::viewport::ViewState;
#[cfg(feature = "screenshot")]
use smol_str::SmolStr;

const SESSION: &str = "sess-1";
#[cfg(feature = "screenshot")]
const LINE_HEIGHT: f32 = 18.0;
#[cfg(feature = "screenshot")]
const FONT_SIZE: f32 = 14.0;
#[cfg(feature = "screenshot")]
const VIEW_W: f32 = 760.0;
#[cfg(feature = "screenshot")]
const VIEW_H: f32 = 460.0;

/// What a scenario builder leaves in the layered doc.
struct Built {
    doc_id: String,
    path: String,
    session: String,
}

type Builder = fn(&LayeredDoc) -> Result<Built>;

const SCENARIOS: &[(&str, Builder)] = &[
    ("agent_insert_midfile", s_insert_midfile),
    ("agent_insert_top", s_insert_top),
    ("agent_append_end", s_append_end),
    ("agent_modify_line", s_modify_line),
    ("agent_delete_line", s_delete_line),
    ("agent_multi_hunk", s_multi_hunk),
    ("user_agent_disjoint", s_user_agent_disjoint),
    ("user_agent_conflict", s_user_agent_conflict),
    ("sync_external_then_agent", s_sync_then_agent),
    ("agent_insert_drift", s_insert_drift),
];

fn main() -> Result<()> {
    #[cfg(feature = "screenshot")]
    let mut wgpu_ok = true;
    for (name, build) in SCENARIOS {
        let tmp = tempfile::tempdir()?; // fresh vault per scenario
        let log = LayeredDoc::open(tmp.path())?;
        let built = build(&log)?;

        let accepted = log.materialize_accepted(&built.doc_id)?.text;
        let working = log.materialize_working(&built.doc_id)?.text;
        let proposal = log
            .materialize_review(&built.doc_id, Some(&built.session))?
            .text;

        let plans = overlay_plan(&log, &built, &working, &accepted, &proposal);
        report(name, &accepted, &working, &proposal, &plans);

        #[cfg(feature = "screenshot")]
        if wgpu_ok {
            let out = std::path::PathBuf::from(format!("target/diff-overlay-{name}.png"));
            match render_png(&working, &proposal, &plans, &out) {
                Ok((w, h)) => println!("  screenshot: {} ({w}x{h})", out.display()),
                Err(e) => {
                    println!("  screenshot skipped: {e}");
                    wgpu_ok = false;
                }
            }
        } else {
            println!("  screenshot skipped (no wgpu device)");
        }
        #[cfg(not(feature = "screenshot"))]
        println!("  (screenshots off — build with: cargo run -p diff-overlay-snapshot --features screenshot)");
        println!();
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// Overlay plan — the per-hunk decisions, computed via the real app functions.
// ----------------------------------------------------------------------------

#[derive(Clone)]
enum RowKind {
    /// Plain agent hunk; `drifted` greys Accept (matches the app).
    AcceptReject { drifted: bool },
    /// User also edited this region (conflict): Keep mine/theirs/both.
    Conflict,
    /// No pending op covers this hunk — the app skips verbs entirely; the
    /// change still paints as a diff but there's no action row.
    Skip,
}

#[derive(Clone)]
struct RowPlan {
    /// Anchor byte for the action-row block — only consumed by `combined_set`
    /// (the screenshot path), so it reads as dead code without that feature.
    #[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
    anchor: usize,
    side: BlockSide,
    kind: RowKind,
    // For the report:
    hunk_kind: HunkKind,
    byte_start: usize,
    byte_end: usize,
    op_start: usize,
    op_end: usize,
    working_line: usize,
    anchor_line: usize,
    op_count: usize,
}

fn overlay_plan(
    log: &LayeredDoc,
    built: &Built,
    working: &str,
    accepted: &str,
    proposal: &str,
) -> Vec<RowPlan> {
    let editor = Editor::new(working);
    let rope = &editor.doc;
    let hunks = editor_core::diff::lines(working, proposal);
    let agent_hunks = ov::agent_hunks(rope, proposal, &hunks);
    let user_edits = conflict::user_edit_ranges(accepted, working);
    let session = Some(built.session.as_str());

    let mut plans = Vec::new();
    for ah in &agent_hunks {
        let (anchor, side) = ov::anchor_and_side(rope, ah.byte_start, ah.byte_end);
        // The exact layered-doc seam the app rides: which pending ops cover this hunk.
        let op_ids = op_writes::ops_in_hunk(log, &built.path, session, ah.op_start, ah.op_end)
            .unwrap_or_default();
        let conflict =
            conflict::hunk_overlaps_user_edit(&(ah.byte_start..ah.byte_end), &user_edits);
        let kind = if op_ids.is_empty() {
            RowKind::Skip
        } else if conflict {
            RowKind::Conflict
        } else {
            let drifted = op_ids
                .iter()
                .any(|id| log.is_pending_drifted(&built.doc_id, id).unwrap_or(false));
            RowKind::AcceptReject { drifted }
        };
        plans.push(RowPlan {
            anchor,
            side,
            kind,
            hunk_kind: ah.kind.clone(),
            byte_start: ah.byte_start,
            byte_end: ah.byte_end,
            op_start: ah.op_start,
            op_end: ah.op_end,
            working_line: rope.byte_to_line(ah.byte_start.min(rope.len_bytes())),
            anchor_line: rope.byte_to_line(anchor.min(rope.len_bytes())),
            op_count: op_ids.len(),
        });
    }
    plans
}

/// The combined decoration set (green proposal blocks + action rows). Mirrors
/// the app's `attach_agent_hunk_widgets`: skip-kind hunks get no row.
#[cfg(feature = "screenshot")]
fn combined_set(working: &str, proposal: &str, plans: &[RowPlan]) -> Set {
    let editor = Editor::new(working);
    let rope = &editor.doc;
    let hunks = editor_core::diff::lines(working, proposal);
    let theme = editor_core::theme::light_default();
    let green = proposal_decorations(rope, proposal, &hunks, LINE_HEIGHT, Some(&theme), true);

    let mut entries: Vec<(Range<usize>, Decoration)> =
        green.iter_all().map(|(r, d)| (r, d.clone())).collect();
    let mut next_id: u64 = 1;
    for p in plans {
        let row = match p.kind {
            RowKind::Skip => continue,
            RowKind::AcceptReject { drifted } => accept_reject_row(&mut next_id, p.side, drifted),
            RowKind::Conflict => conflict_row(&mut next_id, p.side),
        };
        entries.push((p.anchor..p.anchor, Decoration::Block(row)));
    }
    RangeSet::from_iter(entries)
}

#[cfg(feature = "screenshot")]
fn btn(next_id: &mut u64, label: &'static str, style: ActionButtonStyle, enabled: bool) -> ActionButton {
    let id = *next_id;
    *next_id += 1;
    ActionButton { id, label: SmolStr::new_static(label), style, enabled }
}

#[cfg(feature = "screenshot")]
fn accept_reject_row(next_id: &mut u64, side: BlockSide, drifted: bool) -> BlockDeco {
    BlockDeco {
        side,
        height: 24.0,
        kind: BlockKind::ActionRow {
            label: SmolStr::new_static(if drifted { "drifted" } else { "" }),
            glyph: None,
            tone: ActionTone::Normal,
            buttons: vec![
                btn(next_id, "Accept", ActionButtonStyle::Primary, !drifted),
                btn(next_id, "Reject", ActionButtonStyle::Danger, true),
            ],
        },
    }
}

#[cfg(feature = "screenshot")]
fn conflict_row(next_id: &mut u64, side: BlockSide) -> BlockDeco {
    BlockDeco {
        side,
        height: 24.0,
        kind: BlockKind::ActionRow {
            label: SmolStr::new_static("conflict"),
            glyph: None,
            tone: ActionTone::Conflicted,
            buttons: vec![
                btn(next_id, "Keep mine", ActionButtonStyle::Primary, true),
                btn(next_id, "Keep theirs", ActionButtonStyle::Danger, true),
                btn(next_id, "Keep both", ActionButtonStyle::Neutral, true),
            ],
        },
    }
}

// ----------------------------------------------------------------------------
// Text report.
// ----------------------------------------------------------------------------

fn report(name: &str, accepted: &str, working: &str, proposal: &str, plans: &[RowPlan]) {
    println!("================ scenario: {name} ================");
    if accepted != working {
        println!("--- accepted (canonical) ---");
        dump(accepted);
    }
    println!("--- working (editable buffer = diff left side) ---");
    dump(working);
    println!("--- proposal (working + pending = diff right side) ---");
    dump(proposal);

    println!("--- agent-overlay hunks ---");
    if plans.is_empty() {
        println!("  (no change hunks)");
    }
    for (i, p) in plans.iter().enumerate() {
        let side = match p.side {
            BlockSide::Above => "Above",
            BlockSide::Below => "Below",
        };
        let kind = match &p.kind {
            RowKind::AcceptReject { drifted: true } => "Accept(disabled,drifted)/Reject",
            RowKind::AcceptReject { drifted: false } => "Accept/Reject",
            RowKind::Conflict => "Keep mine/theirs/both",
            RowKind::Skip => "(no row — no op covers hunk)",
        };
        println!(
            "  #{i} {:?}: working bytes {}..{} (line {}), proposal bytes {}..{}, ops={}",
            p.hunk_kind, p.byte_start, p.byte_end, p.working_line, p.op_start, p.op_end, p.op_count
        );
        println!(
            "      row: {kind} | {side} working line {} ({:?})",
            p.anchor_line,
            line_text(working, p.anchor_line)
        );
        if p.byte_start == p.byte_end && !matches!(p.kind, RowKind::Skip) {
            let aligned = matches!(p.side, BlockSide::Above) && p.anchor_line == p.working_line;
            println!(
                "      {}",
                if aligned {
                    format!("OK: row sits in the same gap as the green addition (above line {}).", p.working_line)
                } else {
                    format!(">>> MISALIGNED: green above line {} but row is {side} line {}.", p.working_line, p.anchor_line)
                }
            );
        }
    }
}

fn dump(text: &str) {
    for (i, line) in text.split('\n').enumerate() {
        println!("  {i:>3} |{line}");
    }
}

fn line_text(text: &str, line: usize) -> String {
    text.split('\n').nth(line).unwrap_or("").to_string()
}

// ----------------------------------------------------------------------------
// Screenshot.
// ----------------------------------------------------------------------------

#[cfg(feature = "screenshot")]
fn render_png(working: &str, proposal: &str, plans: &[RowPlan], out: &Path) -> Result<(u32, u32), String> {
    let renderer = std::panic::catch_unwind(AssertUnwindSafe(
        egui_kittest::wgpu::WgpuTestRenderer::new,
    ))
    .map_err(|_| "wgpu backend unavailable (no GPU/software device)".to_string())?;

    let working = working.to_string();
    let proposal = proposal.to_string();
    let plans = plans.to_vec();

    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::Vec2::new(VIEW_W, VIEW_H))
        .renderer(renderer)
        .build_ui(move |ui| {
            let mut editor = Editor::new(&working);
            let mut view = ViewState {
                font_size: FONT_SIZE,
                line_height: LINE_HEIGHT,
                width: VIEW_W,
                height: VIEW_H,
                ..Default::default()
            };
            view.wrap_map.set_enabled(false);
            view.sync_to(&editor);

            view.decorations.clear();
            view.decorations.push_with_heights(combined_set(&working, &proposal, &plans));

            let mut paint_cache = PaintCache::default();
            let rect = egui::Rect::from_min_size(ui.min_rect().min, egui::vec2(VIEW_W, VIEW_H));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
            EditorWidget::new(&mut editor, &mut view)
                .with_paint_cache(&mut paint_cache)
                .show(&mut child);
        });

    harness.run();
    let render = std::panic::catch_unwind(AssertUnwindSafe(|| harness.render()));
    match render {
        Ok(Ok(image)) => {
            let (w, h) = (image.width(), image.height());
            image.save(out).map_err(|e| format!("save png: {e}"))?;
            Ok((w, h))
        }
        Ok(Err(e)) => Err(format!("wgpu render failed: {e}")),
        Err(_) => Err("wgpu render panicked".into()),
    }
}

// ----------------------------------------------------------------------------
// Scenario builders.
// ----------------------------------------------------------------------------

fn mkdoc(log: &LayeredDoc, path: &str, text: &str) -> Result<String> {
    Ok(log.create_document(path, "note", text, &Author::User)?)
}

fn agent(log: &LayeredDoc, doc_id: &str, edits: &[(Option<&str>, &str)]) -> Result<Vec<String>> {
    let specs: Vec<EditSpec> = edits
        .iter()
        .map(|(o, n)| EditSpec { old_str: o.map(str::to_string), new_str: (*n).to_string() })
        .collect();
    let ctx = ProducerCtx {
        author: Author::Agent("demo".into()),
        surface: "snapshot".into(),
        session_id: Some(SESSION.to_string()),
    };
    Ok(log.stage_pending(doc_id, &specs, &ctx)?.op_ids)
}

fn user_replace(log: &LayeredDoc, doc_id: &str, needle: &str, repl: &str) -> Result<()> {
    let cur = log.materialize_working(doc_id)?.text;
    let off = cur.find(needle).unwrap_or_else(|| panic!("needle {needle:?} not found"));
    log.apply_working_edit(doc_id, off, needle.len(), repl)?;
    Ok(())
}

fn built(doc_id: String, path: &str, _op_ids: Vec<String>) -> Built {
    Built { doc_id, path: path.to_string(), session: SESSION.to_string() }
}

const FM_NOTE: &str =
    "---\ncreated: '2025-01-23'\nstatus: draft\ntags:\n- test\n- example\nhiker:\n  author: human\n---\n# Another Test Note\n\nbody line one\n";
const BODY_NOTE: &str = "alpha\nbravo\ncharlie\ndelta\n";

fn s_insert_midfile(log: &LayeredDoc) -> Result<Built> {
    let p = "midfile.md";
    let d = mkdoc(log, p, FM_NOTE)?;
    let ops = agent(log, &d, &[(Some("- example\n"), "- example\n- demo-agent\n")])?;
    Ok(built(d, p, ops))
}

fn s_insert_top(log: &LayeredDoc) -> Result<Built> {
    let p = "top.md";
    let d = mkdoc(log, p, BODY_NOTE)?;
    let ops = agent(log, &d, &[(Some("alpha\n"), "# Title\nalpha\n")])?;
    Ok(built(d, p, ops))
}

fn s_append_end(log: &LayeredDoc) -> Result<Built> {
    let p = "end.md";
    let d = mkdoc(log, p, BODY_NOTE)?;
    let ops = agent(log, &d, &[(Some("delta\n"), "delta\nomega\n")])?;
    Ok(built(d, p, ops))
}

fn s_modify_line(log: &LayeredDoc) -> Result<Built> {
    let p = "modify.md";
    let d = mkdoc(log, p, BODY_NOTE)?;
    let ops = agent(log, &d, &[(Some("bravo"), "bravo-EDITED")])?;
    Ok(built(d, p, ops))
}

fn s_delete_line(log: &LayeredDoc) -> Result<Built> {
    let p = "delete.md";
    let d = mkdoc(log, p, BODY_NOTE)?;
    let ops = agent(log, &d, &[(Some("bravo\n"), "")])?;
    Ok(built(d, p, ops))
}

fn s_multi_hunk(log: &LayeredDoc) -> Result<Built> {
    let p = "multi.md";
    let d = mkdoc(log, p, BODY_NOTE)?;
    let ops = agent(
        log,
        &d,
        &[(Some("alpha"), "ALPHA-edited"), (Some("delta\n"), "delta\nomega\n")],
    )?;
    Ok(built(d, p, ops))
}

fn s_user_agent_disjoint(log: &LayeredDoc) -> Result<Built> {
    let p = "disjoint.md";
    let d = mkdoc(log, p, BODY_NOTE)?;
    let ops = agent(log, &d, &[(Some("charlie"), "charlie-AGENT")])?;
    user_replace(log, &d, "alpha", "alpha-USER")?;
    Ok(built(d, p, ops))
}

fn s_user_agent_conflict(log: &LayeredDoc) -> Result<Built> {
    let p = "conflict.md";
    let d = mkdoc(log, p, BODY_NOTE)?;
    let ops = agent(log, &d, &[(Some("bravo"), "bravo-AGENT")])?;
    user_replace(log, &d, "bravo", "bravo-USER")?;
    Ok(built(d, p, ops))
}

fn s_sync_then_agent(log: &LayeredDoc) -> Result<Built> {
    let p = "sync.md";
    let d = mkdoc(log, p, "# Notes\nalpha\nbravo\ncharlie\n")?;
    let ops = agent(log, &d, &[(Some("alpha"), "alpha-AGENT")])?;
    log.apply_external_edit(&d, "# Notes\nalpha\nbravo\ncharlie-SYNCED\n")?;
    Ok(built(d, p, ops))
}

fn s_insert_drift(log: &LayeredDoc) -> Result<Built> {
    let p = "drift.md";
    let d = mkdoc(log, p, "alpha\nbravo\ncharlie\n")?;
    let ops = agent(log, &d, &[(Some("bravo\n"), "bravo\nINSERTED\n")])?;
    log.apply_external_edit(&d, "alpha\nBRAVO-CHANGED\ncharlie\n")?;
    Ok(built(d, p, ops))
}
