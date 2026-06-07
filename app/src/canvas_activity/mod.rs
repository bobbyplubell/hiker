//! Canvas activity — a sidebar `Activity` listing every `.canvas`
//! document in the vault. Its single `View` enumerates the vault's
//! canvases (via `panels::canvas::list_canvases`, sorted, vault-relative)
//! and renders each title as a clickable row; clicking defers
//! `panels::canvas::open` — the SAME opener the file tree uses — so the
//! canvas appears in the existing `TabKind::Canvas` tab. No new tab kind:
//! this is a list-only summon of the canvas-tab machinery. The listing is
//! read fresh each frame, so the activity carries no real state; the
//! zero-field `State` marker keeps the registry's `AppCtx::session` seam uniform.

use eframe::egui;

use egui_workbench::activity::{Activity, View};
use crate::activity::{AppCtx, SurfaceCtx};
use crate::icons;
use hiker_theme as theme;

/// Per-activity UI state for the Canvases sidebar. The view is
/// effectively stateless — the listing is read fresh from disk each frame
/// — but the registry's `AppCtx::session` hands every activity a `&mut dyn Any`
/// state slice, so a zero-field marker keeps the seam uniform. Owned by
/// `AppState::canvases_activity_state` (top-level, per
/// `feature-state-ownership`).
#[derive(Default)]
pub struct State;

/// Render the canvas listing through the narrow activity `SurfaceCtx`. Each row
/// is a clickable title; clicking queues `panels::canvas::open` via
/// `ctx.defer` (opening a tab needs full `&mut AppState`).
fn render_body(ui: &mut egui::Ui, ctx: &mut SurfaceCtx<'_>) {
    let canvases = crate::panels::canvas::list_canvases(ctx.vault);

    if canvases.is_empty() {
        ui.add_space(8.0);
        ui.label(egui::RichText::new("No canvases in this vault").color(theme::muted()).small());
        return;
    }

    let mut to_open: Option<String> = None;
    let vault_root = ctx.vault.root().to_path_buf();
    for (rel, title) in &canvases {
        let resp = ui
            .horizontal(|ui| {
                // Inline rich preview of the canvas shape (`preview-canvas-thumbnail`),
                // hover-expandable; the generic widget owns rendering + caching.
                if let Ok(bytes) = ctx.vault.read_file(rel) {
                    let provider = crate::panels::canvas::thumbnail::CanvasPreview::new(bytes);
                    crate::widgets::preview::thumbnail(
                        ui,
                        &provider,
                        &vault_root,
                        crate::widgets::preview::ThumbnailOpts::default(),
                    );
                    ui.add_space(4.0);
                }
                ui.add(
                    egui::Label::new(egui::RichText::new(title).small())
                        .sense(egui::Sense::click()),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(rel.as_str())
            })
            .inner;
        // Same base right-click menu as every other note/doc list row. "Open"
        // routes through `open_file`, which sends `.canvas` paths to the canvas
        // view (the same opener as a click); Reveal / Copy path / Properties
        // act on the `.canvas` file directly.
        crate::item_menu::attach_note_item_menu(
            &resp,
            ctx,
            rel,
            crate::item_menu::BaseOpts { reveal: true },
        );
        if resp.clicked() {
            to_open = Some(rel.clone());
        }
    }

    if let Some(rel) = to_open {
        ctx.defer(move |state| {
            crate::panels::canvas::open(state, &rel);
        });
    }
}

// ---- Activity impl ----------------------------------------------------

/// Zero-sized `Activity` descriptor for the Canvases panel. State lives in
/// `AppState::canvases_activity_state`; the view reads the listing fresh
/// from disk and defers opening to `&mut AppState`.
pub struct CanvasActivity;

impl Activity<dyn AppCtx> for CanvasActivity {
    fn id(&self) -> &'static str {
        "canvases"
    }
    fn label(&self) -> &'static str {
        "Canvases"
    }
    fn icon(&self) -> egui::Image<'static> {
        icons::ICONS.image(icons::Icon::Canvas)
    }
    fn views(&self) -> Vec<&dyn View<dyn AppCtx>> {
        vec![&CanvasListView]
    }
}

struct CanvasListView;

impl View<dyn AppCtx> for CanvasListView {
    fn id(&self) -> &'static str {
        "canvases"
    }
    fn render(&self, ui: &mut egui::Ui, ctx: &mut (dyn AppCtx + 'static)) {
        let Some(mut ctx) = ctx.surface_ctx(self.state_key()) else {
            return;
        };
        let ctx = &mut ctx;
        egui::ScrollArea::vertical()
            .id_salt("panel-canvases-body")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                render_body(ui, ctx);
            });
    }
}
