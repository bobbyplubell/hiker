//! Tiny status window for the puffin profiler — toggled with F12.
//!
//! The actual flame graph lives in the external `puffin_viewer` binary
//! (connect to 127.0.0.1:8585). This overlay just tells the user
//! whether collection is on, how to connect, and which build flavour
//! they're running.

use eframe::egui;

use crate::state::AppState;

pub fn show(ctx: &egui::Context, state: &mut AppState) {
    if !state.ui.show_profiler {
        return;
    }
    let mut open = true;
    egui::Window::new("Profiler")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(380.0)
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 96.0))
        .show(ctx, |ui| {
            #[cfg(feature = "profiling")]
            {
                let enabled = crate::profiling::is_enabled();
                let mut on = enabled;
                ui.checkbox(&mut on, "Collect frames");
                if on != enabled {
                    crate::profiling::set_enabled(on);
                }
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("Live flame graph:")
                        .small()
                        .strong(),
                );
                ui.label(
                    egui::RichText::new(
                        "1. Install once: `cargo install puffin_viewer`\n\
                         2. Run: `puffin_viewer --url 127.0.0.1:8585`",
                    )
                    .small()
                    .monospace(),
                );
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "F12 toggles this window. Add new markers with \
                         `crate::profile_function!()` or \
                         `crate::profile_scope!(\"name\")` — they're \
                         zero-cost without the `profiling` feature.",
                    )
                    .small()
                    .italics(),
                );
            }
            #[cfg(not(feature = "profiling"))]
            {
                ui.label(
                    egui::RichText::new(
                        "Built without the profiling feature.\n\
                         Rebuild with `cargo run --features profiling` \
                         (debug or release) to enable.\n\n\
                         The viewer is a separate program:\n\
                         `cargo install puffin_viewer`",
                    )
                    .small(),
                );
            }
        });
    if !open {
        state.ui.show_profiler = false;
    }
}
