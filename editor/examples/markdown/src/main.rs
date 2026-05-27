use std::collections::HashSet;
use std::sync::Arc;

use editor_core::compartment::Compartment;

use editor_core::compartment::Store;
use editor_core::state::Editor;
use editor_core::theme::light_default;


use editor_core::theme::Theme;
use editor_egui::widget::Widget as EditorWidget;
use editor_md::admonitions::callout_decorations;
use editor_md::notes::footnote_decorations;
use editor_md::folds::fold_decorations;
use editor_md::meta::frontmatter_fold;
use editor_md::styling::markdown_decorations;
use editor_md::equations::math_decorations;
use editor_md::diagrams::mermaid_decorations;
use editor_md::embeds::transclusion_decorations;
use editor_md::links::wikilink_decorations;
use editor_md::indenter::MarkdownIndent;
use editor_view::brackets;
use editor_view::highlight::occurrence_decorations;
use editor_view::brackets::bracket_match_decorations;

use editor_view::highlights::active_line_decorations;

use editor_view::highlights::trailing_whitespace_decorations;
use editor_view::viewport::ClickAction;

use editor_view::viewport::ViewState;
const SAMPLE: &str = r#"---
title: egui_editor demo
tags: [markdown, editor, demo]
---
# egui_editor — live preview demo

This editor is rendering **markdown live preview** in egui, with *italic*,
**bold**, ~~strikethrough~~, and `inline code` all styled in place.

Wikilinks like [[Home]] or [[Inbox|today's inbox]] render as styled chips.

Transclusions like ![[OtherNote]] render as a chip preview.

Inline math: $E = mc^2$ — and a footnote reference[^1] in prose.

[^1]: A footnote definition lives at the start of a line.

```mermaid
graph TD
  A --> B
  B --> C
```


> [!warning] Heads up
> Callout blockquotes get a colored bar and tinted background.
> The `> ` marker is recolored to match the type.

## Lists

- first item
  - nested first
  - nested second
- second item with **bold** inside
- third item

## A blockquote

> note: source markers hide when the cursor leaves the line — click into the
> heading above and watch the `#` reappear.

## Code block

```rust
fn main() {
    println!("hello");
}
```

## Tasks

- [ ] write the spec
- [x] build the rope
- [x] build the egui widget
- [ ] ship it

A [link to the repo](https://example.com) demonstrates link styling.

## Folding

Click the ▼ in the gutter beside any heading (or beside a list item with
nested children) to collapse the section underneath it. The chevron flips to
▶ when collapsed; click again to expand.

Try selecting a word — every other occurrence of that word in the visible
viewport gets highlighted.
"#;

struct App {
    state: Editor,
    view: ViewState,
    folds: HashSet<u64>,
    click_buffer: Vec<ClickAction>,
    /// Theme compartment + a local store. In a real app, the store would
    /// live on `Editor` (it does — see `state.compartments` via
    /// `Store`), but we keep a separate store here to keep the
    /// example self-contained while StateField wiring lands.
    theme_compartment: Compartment<Theme>,
    theme_store: Store,
}

impl Default for App {
    fn default() -> Self {
        let theme_compartment: Compartment<Theme> = Compartment::new();
        let mut theme_store = Store::default();
        theme_store.set(&theme_compartment, light_default());

        Self {
            state: Editor::new(SAMPLE),
            view: {
                let mut v = ViewState {
                    font_size: 15.0,
                    indent_provider: Some(Arc::new(MarkdownIndent)),
                    placeholder: Some("Start typing markdown…".into()),
                    scroll_past_end: 0.3,
                    ..ViewState::default()
                };
                v.wrap_map.set_enabled(true);
                v
            },
            folds: HashSet::new(),
            click_buffer: Vec::new(),
            theme_compartment,
            theme_store,
        }
    }
}

impl App {
    fn drain_clicks(&mut self) {
        for action in self.click_buffer.drain(..) {
            match action {
                ClickAction::ToggleFold(id) => {
                    if !self.folds.remove(&id) {
                        self.folds.insert(id);
                    }
                }
                ClickAction::WidgetClick(_) => {}
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Pull the active theme from the compartment store. Hosts can swap
        // themes via `theme_store.reconfigure(&theme_compartment, …)` and
        // every provider below picks up the new colors on the next frame.
        let theme = self.theme_store.get(&self.theme_compartment);

        // Build decoration layers fresh from current state + fold set.
        self.view.decorations.clear();
        self.view
            .decorations
            .push(active_line_decorations(&self.state));
        self.view
            .decorations
            .push(trailing_whitespace_decorations(&self.state, None));
        self.view
            .decorations
            .push(markdown_decorations(&self.state, theme));
        self.view
            .decorations
            .push(fold_decorations(&self.state, &self.folds));
        self.view
            .decorations
            .push(wikilink_decorations(&self.state, theme, None, None));
        self.view
            .decorations
            .push(callout_decorations(&self.state, theme, None));
        self.view
            .decorations
            .push(frontmatter_fold(&self.state, &self.folds, theme));
        self.view
            .decorations
            .push(transclusion_decorations(&self.state, theme, None));
        self.view
            .decorations
            .push(footnote_decorations(&self.state, theme, None));
        self.view.decorations.push(math_decorations(&self.state, theme, None));
        self.view
            .decorations
            .push(mermaid_decorations(&self.state, theme, None));

        let visible = self.view.visible_lines();
        let last_line = self.state.doc.len_lines().saturating_sub(1);
        let visible_start = self
            .state
            .doc
            .line_to_byte(visible.start.min(last_line));
        let visible_end_line = visible.end.min(last_line);
        let visible_end = if visible_end_line + 1 < self.state.doc.len_lines() {
            self.state.doc.line_to_byte(visible_end_line + 1)
        } else {
            self.state.doc.len_bytes()
        };
        self.view
            .decorations
            .push(occurrence_decorations(&self.state, visible_start..visible_end));
        self.view
            .decorations
            .push(bracket_match_decorations(&self.state, brackets::DEFAULT_BRACKETS, 5000));

        egui::CentralPanel::default().show(ctx, |ui| {
            let click_buffer = &mut self.click_buffer;
            EditorWidget::new(&mut self.state, &mut self.view)
                .with_click_sink(click_buffer)
                .show(ui);
        });

        self.drain_clicks();
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 700.0])
            .with_title("egui_editor — markdown live preview"),
        ..Default::default()
    };
    eframe::run_native(
        "egui_editor markdown",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::light());
            Ok(Box::new(App::default()))
        }),
    )
}
