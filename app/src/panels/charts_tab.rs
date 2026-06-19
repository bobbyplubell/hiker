//! The chart-builder tab: the `hiker-charts` builder panel + live preview, over
//! either a `.csv` file or an inline ```` ```chart ```` block opened from a note.
//!
//! The whole UI — control column, interactive chart preview, data grid — is the
//! pure-egui [`hiker_charts_gui::panel::panel`]; this module is the thin host
//! glue: seed a [`BuilderState`] from the source, carry the builder + camera +
//! preview-view + theme across frames (keyed by [`ChartSource::pane_key`]), and
//! route the result. A `.csv` source's Export copies a ```` ```chart ```` block
//! to the clipboard; a note-block source's Save splices the regenerated block
//! back into the note. status: chart-csv-tab, chart-open-in-builder

use std::ops::Range;

use eframe::egui;
use hiker_charts_core::backend::Size;
use hiker_charts_core::data::Table;
use hiker_charts_core::dsl::ChartSpec;
use hiker_charts_core::host::DataResolver;
use hiker_charts_gui::camera::Camera;
use hiker_charts_gui::model::{BuilderState, Provenance};
use hiker_charts_gui::panel::ThemeChoice;
use hiker_charts_gui::preview::View;

use crate::charts::VaultDataResolver;
use crate::state::{AppState, ToastLevel};
use crate::tab::{ChartSource, TabKind};

/// The canvas size the builder renders at. The preview auto-fits, so this is the
/// rendered aspect, not a hard limit.
const BUILDER_SIZE: Size = Size { width: 960, height: 600 };

/// What a builder pane saves to, and the data it needs to do so.
enum PaneKind {
    /// A `.csv` opened directly — Export copies a ```chart block to the clipboard.
    Csv,
    /// An inline block opened from a note — Save splices the regenerated block
    /// back. `original_inner` is the block body at open (or after the last save),
    /// used to re-locate the fence in the live note so an edit elsewhere doesn't
    /// misplace the write. status: chart-open-in-builder
    Note { note: String, original_inner: String },
}

/// Per-tab builder state, carried across frames (keyed by [`ChartSource::pane_key`]
/// in [`crate::state::PanelStates::chart_builders`]). status: chart-csv-tab
pub struct Pane {
    builder: BuilderState,
    camera: Camera,
    view: View,
    theme: ThemeChoice,
    kind: PaneKind,
}

/// Seed a sensible default spec for a just-opened CSV: a bar chart with `x` =
/// the first column and `y` = the second (when present). Falls back to a bare
/// `mark: bar` if a column name can't be expressed as inline YAML.
fn default_spec(table: &Table) -> ChartSpec {
    let cols: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    let mut yaml = String::from("mark: bar\n");
    if let Some(x) = cols.first() {
        yaml.push_str(&format!("x: {x}\n"));
    }
    if let Some(y) = cols.get(1) {
        yaml.push_str(&format!("y: {y}\n"));
    }
    ChartSpec::from_yaml(&yaml)
        .unwrap_or_else(|_| ChartSpec::from_yaml("mark: bar\n").expect("minimal bar spec parses"))
}

/// The chart theme a fresh [`ThemeChoice`] represents.
fn chart_theme(theme: ThemeChoice) -> hiker_charts_core::theme::Theme {
    hiker_charts_core::theme::Theme::from_dark_mode(theme.dark).with_palette(theme.palette)
}

/// Build a CSV-source pane over a freshly-loaded `table`.
fn csv_pane(table: Table) -> Pane {
    let theme = ThemeChoice::default();
    let builder = BuilderState::new(default_spec(&table), table, chart_theme(theme), BUILDER_SIZE);
    Pane { builder, camera: Camera::default(), view: View::new(), theme, kind: PaneKind::Csv }
}

/// Build a note-block-source pane from a fence body, resolving an external
/// `data:` reference through `resolver` when the block carries no inline CSV.
fn note_pane(
    inner: &str,
    note: &str,
    inner_range: Range<usize>,
    resolver: &VaultDataResolver,
) -> Result<Pane, String> {
    let theme = ThemeChoice::default();
    let ct = chart_theme(theme);
    let prov = Provenance { note_id: note.to_string(), byte_range: inner_range };
    // An inline `---` section is self-contained; otherwise resolve the `data:`
    // reference to a table and open via `from_block` (config-only save-back).
    let builder = if hiker_charts_core::block::split_block(inner).1.is_some() {
        BuilderState::from_block_body(inner, prov, ct, BUILDER_SIZE)?
    } else {
        let spec = ChartSpec::from_yaml(inner).map_err(|e| e.to_string())?;
        let data_id = spec.data.as_deref().ok_or("chart has no inline data and no `data:` reference")?;
        let table = resolver.resolve(data_id).map_err(|e| e.to_string())?;
        BuilderState::from_block(inner, table, None, prov, ct, BUILDER_SIZE)?
    };
    Ok(Pane {
        builder,
        camera: Camera::default(),
        view: View::new(),
        theme,
        kind: PaneKind::Note { note: note.to_string(), original_inner: inner.to_string() },
    })
}

/// Open an inline ```` ```chart ```` block (at `inner_range` in `note`, with body
/// `inner`) in the builder, find-or-focusing its tab and seeding the pane. The
/// note-bound [`VaultDataResolver`] resolves an external `data:` reference so a
/// non-inline block opens too. status: chart-open-in-builder
pub fn open_block(app: &mut AppState, note: &str, inner: &str, inner_range: Range<usize>) {
    let source = ChartSource::NoteBlock { note: note.to_string(), key: inner_range.start.to_string() };
    let pane_key = source.pane_key();
    if !app.panels.chart_builders.contains_key(&pane_key) {
        let resolver = VaultDataResolver::new(app.vault_session.vault.as_ref().clone(), note);
        match note_pane(inner, note, inner_range, &resolver) {
            Ok(pane) => {
                app.panels.chart_builders.insert(pane_key, pane);
            }
            Err(err) => {
                app.push_toast(format!("Can't open chart in builder: {err}"), ToastLevel::Error);
                return;
            }
        }
    }
    let want = source.clone();
    app.find_or_open_tab(
        |k| matches!(k, TabKind::ChartBuilder { source: s } if *s == want),
        move || TabKind::ChartBuilder { source },
    );
}

/// Render the chart-builder tab for `source`. Lazily loads a CSV source on first
/// show; a note-block source's pane is created by [`open_block`]. status: chart-csv-tab
pub fn show(ui: &mut egui::Ui, app: &mut AppState, source: &ChartSource) {
    let key = source.pane_key();
    if !app.panels.chart_builders.contains_key(&key) {
        match source {
            ChartSource::Csv { path } => match load_table(app, path) {
                Ok(table) => {
                    app.panels.chart_builders.insert(key.clone(), csv_pane(table));
                }
                Err(err) => {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("Can't open {path} as a chart: {err}"),
                    );
                    return;
                }
            },
            ChartSource::NoteBlock { .. } => {
                ui.label("This chart editor was closed — reopen it by clicking the chart in its note.");
                return;
            }
        }
    }

    // A note-block pane gets a Save bar; gather the save request without holding
    // the `&mut pane` borrow across the AppState mutation below.
    let mut save_request: Option<(String, String, String)> = None;
    // Set to a user-facing message when a button copies a block this frame, so
    // the toast fires after the `&mut pane` borrow (above) is released.
    let mut copied_toast: Option<String> = None;
    {
        let Some(pane) = app.panels.chart_builders.get_mut(&key) else { return };
        match &pane.kind {
            // A note-block builder gets a Save bar (splice the regenerated block
            // back into the note). status: chart-open-in-builder
            PaneKind::Note { note, .. } => {
                let note = note.clone();
                ui.horizontal(|ui| {
                    if ui.button("Save to note").clicked()
                        && let Some((body, _range)) = pane.builder.save_block()
                        && let PaneKind::Note { original_inner, .. } = &pane.kind
                    {
                        save_request = Some((note.clone(), original_inner.clone(), body));
                    }
                    ui.label(format!("Editing chart in {}", basename(&note)));
                });
                ui.separator();
            }
            // A CSV-file builder gets the two "copy as block" modes: a
            // self-contained block (config + inline CSV) or a `data:` reference
            // to the source file. status: chart-export-mode
            PaneKind::Csv => {
                let data_path = source.host_path().to_string();
                ui.horizontal(|ui| {
                    if ui.button("Copy self-contained block").clicked() {
                        ui.ctx().copy_text(pane.builder.to_block_inline());
                        copied_toast = Some("Copied self-contained chart block".to_string());
                    }
                    if ui.button("Copy data: reference block").clicked() {
                        ui.ctx().copy_text(pane.builder.to_block_reference(&data_path));
                        copied_toast =
                            Some(format!("Copied chart block referencing {}", basename(&data_path)));
                    }
                });
                ui.separator();
            }
        }
        // The panel's own "Export chart block" button defaults to a renderable
        // self-contained block (`to_block`), so it stays useful alongside the
        // explicit-mode buttons above.
        let exported = hiker_charts_gui::panel::panel(
            &mut pane.builder,
            &mut pane.theme,
            &mut pane.camera,
            &mut pane.view,
            ui,
        );
        if let Some(block) = exported {
            ui.ctx().copy_text(block);
            copied_toast = Some("Copied chart block to clipboard".to_string());
        }
    }

    if let Some(msg) = copied_toast {
        app.push_toast(msg, ToastLevel::Info);
    }

    if let Some((note, original_inner, body)) = save_request {
        match save_note_block(app, &note, &original_inner, &body) {
            Ok(()) => {
                // Advance the relocation anchor to the just-written body so a
                // second save finds the block again.
                if let Some(pane) = app.panels.chart_builders.get_mut(&key)
                    && let PaneKind::Note { original_inner, .. } = &mut pane.kind
                {
                    *original_inner = body;
                }
                app.push_toast(format!("Saved chart to {}", basename(&note)), ToastLevel::Info);
            }
            Err(err) => app.push_toast(format!("Save failed: {err}"), ToastLevel::Error),
        }
    }
}

/// Re-locate the ```` ```chart ```` fence in `base` to splice into, and return
/// the spliced full text. Finds the fence whose body equals `original_inner`
/// (robust to edits elsewhere in the note); if no body matches but the note has
/// exactly one chart fence, falls back to that one (so a builder still saves
/// after the block was lightly reformatted). Returns `None` when no chart fence
/// can be identified. Pure (no I/O) so the relocation is unit-testable.
/// status: chart-open-in-builder
fn splice_chart_block(base: &str, original_inner: &str, new_body: &str) -> Option<String> {
    let editor = editor_core::state::Editor::new(base);
    let spans = editor_md::diagrams::chart_spans(&editor, None);
    let range = spans
        .iter()
        .find(|s| base.get(s.inner_range.clone()) == Some(original_inner))
        .or_else(|| if spans.len() == 1 { spans.first() } else { None })
        .map(|s| s.inner_range.clone())?;
    let mut new_text = base.to_string();
    new_text.replace_range(range, new_body);
    Some(new_text)
}

/// Splice the regenerated `new_body` back into the ```` ```chart ```` fence in
/// `note` and persist it.
///
/// The write goes through the layered-doc **working** layer (then commits), not
/// straight to `accepted` via `user_save`: when the note is open in a buffer its
/// per-frame editor binding tracks `materialize_working` and would otherwise
/// *revert* an accepted-only out-of-band write on the next frame. Writing to
/// `working` first means the binding pulls the change into the live editor, and
/// the commit persists it to disk — so the chart actually saves whether or not
/// the note is currently open. status: chart-open-in-builder
fn save_note_block(
    app: &mut AppState,
    note: &str,
    original_inner: &str,
    new_body: &str,
) -> Result<(), String> {
    // Base on the live buffer text when the note is open (preserves unsaved
    // edits), else disk.
    let base = match app.session.buffers.get(note) {
        Some(b) => b.current_text(),
        None => app.vault_session.vault.read_file(note).map_err(|e| e.to_string())?,
    };
    let new_text = splice_chart_block(&base, original_inner, new_body)
        .ok_or("couldn't locate the chart block in the note (was it edited?)")?;

    let log = &app.vault_session.services.layered;
    let doc_id = log
        .doc_id_for_path(note)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no op-log document for {note}"))?;
    // Replace the whole working layer with the spliced text, then commit it to
    // `accepted` + disk (mirrors the editor binding's resync-then-save path).
    log.discard_working(&doc_id).map_err(|e| e.to_string())?;
    let accepted_len = log.materialize_accepted(&doc_id).map_err(|e| e.to_string())?.text.len();
    log.apply_working_edit(&doc_id, 0, accepted_len, &new_text).map_err(|e| e.to_string())?;
    log.commit_working(&doc_id).map_err(|e| e.to_string())?;

    // Sync the open buffer's clean baseline so it isn't left falsely dirty; the
    // editor binding pulls the committed text into the live editor next frame.
    if let Some(b) = app.session.buffers.get_mut(note) {
        b.loaded_hash = hiker_core::hash_string(&new_text);
        b.loaded_text = new_text;
    }
    Ok(())
}

/// Read the CSV at `path` through the vault sandbox and parse it to a [`Table`].
fn load_table(app: &AppState, path: &str) -> Result<Table, String> {
    let text = app.vault_session.vault.read_file(path).map_err(|e| e.to_string())?;
    Table::from_csv(text.as_bytes()).map_err(|e| e.to_string())
}

/// Basename of a vault path, for labels.
fn basename(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// Drop the cached builder pane for `pane_key` (called on tab close). status: chart-csv-tab
pub fn forget(app: &mut AppState, pane_key: &str) {
    app.panels.chart_builders.remove(pane_key);
}

#[cfg(test)]
mod tests {
    use super::splice_chart_block;

    #[test]
    fn splice_replaces_matching_chart_block() {
        let base = "intro\n\n```chart\nmark: bar\nx: a\ny: b\n---\na,b\n1,2\n```\n\nmore\n";
        let original_inner = "mark: bar\nx: a\ny: b\n---\na,b\n1,2\n";
        let new_body = "mark: line\nx: a\ny: b\n---\na,b\n1,2\n";
        let out = splice_chart_block(base, original_inner, new_body).expect("located");
        assert!(out.contains("mark: line") && !out.contains("mark: bar"));
        assert!(out.starts_with("intro\n\n```chart\nmark: line"));
        assert!(out.ends_with("```\n\nmore\n"), "fences + surrounding text preserved");
    }

    #[test]
    fn splice_falls_back_to_sole_chart_when_body_drifted() {
        // The stored body no longer matches (the note was lightly reformatted),
        // but there's exactly one chart, so the save still lands.
        let base = "```chart\nmark: bar\nx: a\ny: b\n---\na,b\n1,2\n```\n";
        let out = splice_chart_block(base, "DOES NOT MATCH", "mark: area\n---\na,b\n1,2\n");
        assert!(out.expect("fell back to the sole chart").contains("mark: area"));
    }

    #[test]
    fn splice_picks_the_right_block_among_many() {
        let base = "```chart\nmark: bar\n---\na\n1\n```\n\n```chart\nmark: line\n---\nb\n2\n```\n";
        let out = splice_chart_block(base, "mark: line\n---\nb\n2\n", "mark: arc\n---\nb\n2\n").unwrap();
        // Only the matched (second) block changed; the first is untouched.
        assert!(out.contains("mark: bar"), "first block untouched");
        assert!(out.contains("mark: arc") && !out.contains("mark: line"));
    }

    #[test]
    fn splice_none_when_no_chart_and_no_match() {
        assert!(splice_chart_block("just text, no chart\n", "x", "y").is_none());
    }
}
