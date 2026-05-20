# egui_workbench

IDE-style workbench layout for [egui]: activity bar + side bars +
tabbed editor groups + bottom panel + status bar. Built on [egui_tiles].

[![Crates.io](https://img.shields.io/crates/v/egui_workbench)](https://crates.io/crates/egui_workbench)
[![Docs.rs](https://img.shields.io/docsrs/egui_workbench)](https://docs.rs/egui_workbench)

[egui]: https://github.com/emilk/egui
[egui_tiles]: https://github.com/rerun-io/egui_tiles

## What you get

- Vertical activity bar with reorderable, badged icon items.
- Resizable primary + secondary side bars, swappable left/right.
- Tabbed editor area with split-by-drag, pinned tabs, preview tabs,
  dirty indicators, context menus.
- Toggleable bottom panel area for tools (terminal, output, etc.).
- Cell-based status bar.
- Layout persistence (single versioned JSON document, host-agnostic).
- Drop indicators with theme-accented overlay during drag.
- "All tabs" dropdown per group when the tab strip overflows.
- Keyboard navigation hooks (`focus_group`, `next_tab_in_group`,
  `close_active`, etc.) for hosts to wire to their command palette.

## Quick start

```rust,ignore
use eframe::egui;
use egui_workbench::{
    DocumentTab, OpenTabOptions, TabUiContext, Workbench, WorkbenchBehavior,
};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
enum MyTab {
    File(String),
    Settings,
}

impl DocumentTab for MyTab {
    fn title(&self) -> egui::WidgetText {
        match self {
            MyTab::File(p) => p.clone().into(),
            MyTab::Settings => "Settings".into(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
enum MyMode { Files }

struct MyBehavior;

impl WorkbenchBehavior<MyTab, MyMode> for MyBehavior {
    fn pane_ui(&mut self, ui: &mut egui::Ui, tab: &mut MyTab, _ctx: TabUiContext<'_>) {
        match tab {
            MyTab::File(path) => { ui.label(&*path); }
            MyTab::Settings => { ui.heading("Settings"); }
        }
    }
}

struct App {
    workbench: Workbench<MyTab, MyMode>,
}

impl Default for App {
    fn default() -> Self {
        let mut workbench = Workbench::<MyTab, MyMode>::new();
        workbench.open_tab(
            MyTab::File("README.md".into()),
            OpenTabOptions::default(),
        );
        Self { workbench }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut behavior = MyBehavior;
        self.workbench.ui(ctx, &mut behavior);
    }
}
```

See `examples/workbench-demo/src/main.rs` for a full runnable demo.

## Layout persistence

`Workbench::layout()` returns a serializable `WorkbenchLayout`. The
schema is versioned independently from the crate version; v1 is the
initial format. To restore, parse the stored JSON into a
`serde_json::Value` and pass it through `migrate()` before applying:

```rust,ignore
let json = std::fs::read_to_string("layout.json")?;
let value: serde_json::Value = serde_json::from_str(&json)?;
if let Some(layout) = egui_workbench::migrate(value) {
    workbench.apply_layout(layout)?;
}
```

Unknown schema versions return `None` (logged as a warning) rather
than panicking, so a stale on-disk layout cannot crash the host.

## Theming

The default `WorkbenchTheme` derives every value from the ambient
`egui::Style`. Override the few extras (activity bar background,
accent color, focused-group border) by implementing
`WorkbenchBehavior::theme`:

```rust,ignore
fn theme(&self, style: &egui::Style) -> egui_workbench::WorkbenchTheme {
    let mut t = egui_workbench::WorkbenchTheme::from_egui_style(style);
    t.accent = egui::Color32::from_rgb(0x00, 0x7a, 0xcc);
    t
}
```

## Examples

- `cargo run -p workbench-demo` — full demo with all features.

## Snapshot tests

The crate ships visual snapshot tests under `tests/snapshots.rs`. They
are `#[ignore]`d by default because they require committed baseline
PNGs. To generate baselines locally:

```sh
UPDATE_SNAPSHOTS=1 cargo test -p egui_workbench --test snapshots -- --ignored
```

Review the generated `tests/snapshots/*.png` files visually before
committing them.

## Status

Pre-1.0. API may shift in minor versions. Layout schema bumps
independently.

## License

Apache-2.0.
