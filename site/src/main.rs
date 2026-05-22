//! Browser-runnable "site as an IDE" built on `egui_workbench`.
//!
//! Pages are loaded from `assets/pages/`, embedded at compile time with
//! `include_dir` (wasm has no runtime filesystem). Each file becomes a tab:
//! `.md` files open in the `editor-egui` widget with markdown live-preview
//! decorations; everything else opens in the plain code editor.
//!
//! Native entry point uses `eframe::run_native`; the wasm entry point mounts
//! onto the `<canvas>` in `index.html` via `eframe::WebRunner`.

use std::collections::HashSet;
use std::sync::Arc;

use eframe::egui;
use editor_core::state::Editor as EditorState;
use editor_core::theme::{Theme, light_default};
use editor_egui::widget::Widget as EditorWidget;
use editor_md::admonitions::callout_decorations;
use editor_md::folds::fold_decorations;
use editor_md::indenter::MarkdownIndent;
use editor_md::links::wikilink_decorations;
use editor_md::styling::markdown_decorations;
use editor_view::highlight::occurrence_decorations;
use editor_view::highlights::active_line_decorations;
use editor_view::viewport::{ClickAction, ViewState};
use egui_workbench::activity_bar::Item;
use egui_workbench::behavior::Host;
use egui_workbench::tab::{Document, UiContext};
use egui_workbench::theme::Palette;
use egui_workbench::workspace::{OpenTabOptions, Workbench};
use include_dir::{Dir, include_dir};
use serde::{Deserialize, Serialize};

/// The pages folder, embedded into the binary at compile time.
static PAGES: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/assets/pages");

// ---------- entry points ----------

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    env_logger::init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("workbench site")
            .with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "workbench-site",
        options,
        Box::new(|cc| Ok(Box::new(SiteApp::new(&cc.egui_ctx)))),
    )
}

#[cfg(target_arch = "wasm32")]
fn main() {
    use eframe::wasm_bindgen::JsCast as _;

    eframe::WebLogger::init(log::LevelFilter::Debug).ok();
    let web_options = eframe::WebOptions::default();
    wasm_bindgen_futures::spawn_local(async {
        let document = web_sys::window()
            .expect("no window")
            .document()
            .expect("no document");
        let canvas = document
            .get_element_by_id("the_canvas_id")
            .expect("missing canvas element")
            .dyn_into::<web_sys::HtmlCanvasElement>()
            .expect("not a canvas element");
        eframe::WebRunner::new()
            .start(
                canvas,
                web_options,
                Box::new(|cc| Ok(Box::new(SiteApp::new(&cc.egui_ctx)))),
            )
            .await
            .expect("failed to start eframe");
    });
}

// Activity-bar icons: 64x64 straight-alpha RGBA, baked from the Feather
// `file-text` / `link` SVGs at dev time (see assets/*.svg). Shipping raw
// pixels means no SVG engine (resvg) in the binary.
const ICON_SIZE: usize = 64;
const PAGE_ICON_RGBA: &[u8] = include_bytes!("../assets/page.rgba");
const LINK_ICON_RGBA: &[u8] = include_bytes!("../assets/link.rgba");

fn load_icon(ctx: &egui::Context, name: &str, rgba: &[u8]) -> egui::TextureHandle {
    let image = egui::ColorImage::from_rgba_unmultiplied([ICON_SIZE, ICON_SIZE], rgba);
    ctx.load_texture(name, image, egui::TextureOptions::LINEAR)
}

// ---------- page model ----------

/// One loaded file. Editor state is per-page and not `Clone`, so pages live
/// in a `Vec` on the app; the `SiteTab` payload just carries an index into it.
struct Page {
    /// File name as shown on the tab (ordering prefix stripped).
    title: String,
    markdown: bool,
    state: EditorState,
    view: ViewState,
    folds: HashSet<u64>,
    clicks: Vec<ClickAction>,
}

/// A markdown-tuned view: soft wrap on, list indenting, comfortable size.
fn markdown_view() -> ViewState {
    let mut v = ViewState {
        font_size: 15.0,
        indent_provider: Some(Arc::new(MarkdownIndent)),
        placeholder: Some("Start typing markdown…".into()),
        scroll_past_end: 0.3,
        ..ViewState::default()
    };
    v.wrap_map.set_enabled(true);
    v
}

/// Strip a leading `NN-` / `NN_` ordering prefix used to sort files, so the
/// tab shows `home.md` rather than `01-home.md`.
fn display_title(file_name: &str) -> String {
    let bytes = file_name.as_bytes();
    let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0
        && bytes
            .get(digits)
            .is_some_and(|b| *b == b'-' || *b == b'_')
    {
        file_name[digits + 1..].to_string()
    } else {
        file_name.to_string()
    }
}

fn load_pages() -> Vec<Page> {
    let mut files: Vec<_> = PAGES.files().collect();
    files.sort_by(|a, b| a.path().cmp(b.path()));
    files
        .into_iter()
        .map(|f| {
            let file_name = f
                .path()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let markdown = f.path().extension().is_some_and(|e| e == "md");
            let contents = f.contents_utf8().unwrap_or("");
            Page {
                title: display_title(&file_name),
                markdown,
                state: EditorState::new(contents),
                view: if markdown {
                    markdown_view()
                } else {
                    ViewState::default()
                },
                folds: HashSet::new(),
                clicks: Vec::new(),
            }
        })
        .collect()
}

/// Rebuild a markdown page's decoration layers from current state. Layers
/// that affect row height (`markdown_decorations` heading scale,
/// `fold_decorations` hidden lines) must use `push_with_heights` so the
/// heightmap driver scans them — `push` alone is paint-only.
fn rebuild_markdown_decorations(page: &mut Page, theme: &Theme) {
    let t = Some(theme);
    page.view.decorations.clear();
    page.view
        .decorations
        .push(active_line_decorations(&page.state));
    page.view
        .decorations
        .push_with_heights(markdown_decorations(&page.state, t));
    page.view
        .decorations
        .push_with_heights(fold_decorations(&page.state, &page.folds));
    page.view
        .decorations
        .push(wikilink_decorations(&page.state, t, None));
    page.view
        .decorations
        .push(callout_decorations(&page.state, t, None));

    let visible = page.view.visible_lines();
    let doc = &page.state.doc;
    let last = doc.len_lines().saturating_sub(1);
    let start = doc.line_to_byte(visible.start.min(last));
    let end = if visible.end.min(last) + 1 < doc.len_lines() {
        doc.line_to_byte(visible.end.min(last) + 1)
    } else {
        doc.len_bytes()
    };
    page.view
        .decorations
        .push(occurrence_decorations(&page.state, start..end));
}

// ---------- app ----------

struct SiteApp {
    workbench: Workbench<SiteTab, Section>,
    pages: Vec<Page>,
    theme: Theme,
    page_icon: egui::TextureHandle,
    link_icon: egui::TextureHandle,
}

impl SiteApp {
    fn new(ctx: &egui::Context) -> Self {
        let pages = load_pages();

        let mut workbench = Workbench::<SiteTab, Section>::new();
        workbench.activity_bar.set_active(Some(Section::Pages));
        for (idx, page) in pages.iter().enumerate() {
            let tab = SiteTab {
                idx,
                title: page.title.clone(),
            };
            let id = workbench.open_tab(tab, &OpenTabOptions::default());
            if idx == 0 {
                workbench.pin_tab(id, true);
            }
        }

        Self {
            workbench,
            pages,
            theme: light_default(),
            page_icon: load_icon(ctx, "page-icon", PAGE_ICON_RGBA),
            link_icon: load_icon(ctx, "link-icon", LINK_ICON_RGBA),
        }
    }
}

impl eframe::App for SiteApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Split the borrow so the per-frame behavior can hold `&mut` to the
        // pages while the workbench (a sibling field) drives the UI.
        let Self {
            workbench,
            pages,
            theme,
            page_icon,
            link_icon,
        } = self;
        let mut behavior = SiteBehavior {
            pages,
            theme,
            page_icon,
            link_icon,
        };
        workbench.ui(ctx, &mut behavior);
    }
}

// ---------- tabs / sections ----------

/// Tab payload: an index into `SiteApp::pages` plus the title to show (the
/// `Document` trait only sees the payload, not the page store).
#[derive(Clone, Serialize, Deserialize)]
struct SiteTab {
    idx: usize,
    title: String,
}

impl Document for SiteTab {
    fn title(&self) -> egui::WidgetText {
        self.title.clone().into()
    }

    // The editor paints its own edge-to-edge surface; drop the standard
    // content inset so there's no strip of pane fill around it.
    fn wants_pane_content_inset(&self) -> bool {
        false
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
enum Section {
    Pages,
    Links,
}

impl Section {
    const fn label(&self) -> &'static str {
        match self {
            Section::Pages => "Pages",
            Section::Links => "Links",
        }
    }
}

// ---------- behavior (host hooks) ----------

struct SiteBehavior<'a> {
    pages: &'a mut Vec<Page>,
    theme: &'a Theme,
    page_icon: &'a egui::TextureHandle,
    link_icon: &'a egui::TextureHandle,
}

impl Host<SiteTab, Section> for SiteBehavior<'_> {
    fn pane_ui(&mut self, ui: &mut egui::Ui, tab: &mut SiteTab, _ctx: UiContext<'_>) {
        let Some(page) = self.pages.get_mut(tab.idx) else {
            ui.label("(missing page)");
            return;
        };

        if page.markdown {
            rebuild_markdown_decorations(page, self.theme);
            EditorWidget::new(&mut page.state, &mut page.view)
                .with_click_sink(&mut page.clicks)
                .show(ui);
            // Apply fold toggles emitted this frame.
            let toggles: Vec<u64> = page
                .clicks
                .drain(..)
                .filter_map(|a| match a {
                    ClickAction::ToggleFold(id) => Some(id),
                    ClickAction::WidgetClick(_) => None,
                })
                .collect();
            for id in toggles {
                if !page.folds.remove(&id) {
                    page.folds.insert(id);
                }
            }
        } else {
            EditorWidget::new(&mut page.state, &mut page.view).show(ui);
        }
    }

    fn side_bar_ui(&mut self, ui: &mut egui::Ui, mode: &Section) {
        match mode {
            Section::Pages => {
                for page in self.pages.iter() {
                    ui.label(&page.title);
                }
            }
            Section::Links => {
                ui.hyperlink_to("GitHub", "https://github.com");
            }
        }
    }

    fn side_bar_title(&self, mode: &Section) -> egui::WidgetText {
        mode.label().into()
    }

    fn theme(&self, style: &egui::Style) -> Palette {
        let mut palette = Palette::from_egui_style(style);
        // Disable the blue "focused editor group" border around the central
        // tab area.
        palette.focused_group_border_width = 0.0;
        palette
    }

    fn status_bar_ui(&mut self, ui: &mut egui::Ui) {
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.label("Rust + egui_workbench + editor-egui → WASM");
        });
    }

    fn activity_items(&self) -> Vec<Item<Section>> {
        vec![
            Item {
                mode: Section::Pages,
                icon: Some(egui::Image::from_texture(self.page_icon)),
                label: "Pages".into(),
                badge: None,
            },
            Item {
                mode: Section::Links,
                icon: Some(egui::Image::from_texture(self.link_icon)),
                label: "Links".into(),
                badge: None,
            },
        ]
    }
}
