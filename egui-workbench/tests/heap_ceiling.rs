//! Steady-state heap regression for the workbench render loop.
//!
//! Builds a workbench with several editor tabs + an active activity,
//! runs warmup frames to settle egui's persistent caches, then resets
//! the peak watermark and drives 500 more frames. Asserts that the
//! peak heap *added* during the steady-state phase stays under a hard
//! ceiling — so a regression that allocates per-frame (forgot to drop
//! a Vec, growing memory cache, etc.) trips the test instead of
//! waiting for the user's process to OOM.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use egui_workbench::tab::Document;

use egui_workbench::workspace::OpenTabOptions;

use egui_workbench::tab::UiContext;

use egui_workbench::workspace::Workbench;

use egui_workbench::behavior::Host;
// ---- Counting allocator -------------------------------------------------

struct CountingAllocator;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let new_cur = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            let mut peak = PEAK.load(Ordering::Relaxed);
            while new_cur > peak {
                match PEAK.compare_exchange_weak(
                    peak,
                    new_cur,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(found) => peak = found,
                }
            }
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

fn cur() -> usize {
    CURRENT.load(Ordering::Relaxed)
}
fn pk() -> usize {
    PEAK.load(Ordering::Relaxed)
}

// ---- Fixture ------------------------------------------------------------

#[derive(Clone)]
struct Tab {
    title: String,
}

impl Document for Tab {
    fn title(&self) -> egui::WidgetText {
        self.title.clone().into()
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Mode {
    Files,
    Search,
}

struct Behavior;
impl Host<Tab, Mode> for Behavior {
    fn pane_ui(&mut self, ui: &mut egui::Ui, tab: &mut Tab, _ctx: UiContext<'_>) {
        ui.label(&tab.title);
    }
    fn side_bar_ui(&mut self, ui: &mut egui::Ui, _mode: &Mode) {
        ui.label("sidebar body");
    }
    fn secondary_side_bar_ui(&mut self, ui: &mut egui::Ui) {
        ui.label("secondary body");
    }
    fn activity_items(&self) -> Vec<egui_workbench::activity_bar::Item<Mode>> {
        vec![
            egui_workbench::activity_bar::Item {
                mode: Mode::Files,
                icon: None,
                label: "Files".into(),
                badge: None,
            },
            egui_workbench::activity_bar::Item {
                mode: Mode::Search,
                icon: None,
                label: "Search".into(),
                badge: None,
            },
        ]
    }
}

// ---- Test ---------------------------------------------------------------

/// Ceiling on heap bytes *added* during the steady-state phase (after
/// warmup, across 500 frames). Egui itself allocates per-frame
/// internally (paint shapes, tessellation, fonts), so this isn't zero.
/// 4 MiB is enough room for that churn but tight enough to flag a
/// workbench-side leak (e.g., a Vec we forgot to drain, an unbounded
/// memory store, a per-frame texture upload).
const STEADY_STATE_CEILING_BYTES: usize = 4 * 1024 * 1024;

#[test]
fn workbench_steady_state_stays_under_ceiling() {
    let mut wb = Workbench::<Tab, Mode>::new();
    wb.activity_bar.set_active(Some(Mode::Files));
    wb.secondary_side_bar.visible = true;
    for i in 0..4 {
        wb.open_tab(
            Tab {
                title: format!("tab-{i}"),
            },
            &OpenTabOptions::default(),
        );
    }

    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1280.0, 800.0))
        .build(|ctx: &egui::Context| {
            let mut beh = Behavior;
            wb.ui(ctx, &mut beh);
        });

    // Warm up: egui builds atlases, fonts, persistent state on first
    // few frames. Don't measure these — they're a fixed one-time cost
    // we don't care about regressing on.
    for _ in 0..16 {
        harness.run();
    }

    // Reset peak to current after warmup, then measure the 500-frame
    // steady-state phase.
    PEAK.store(cur(), Ordering::Relaxed);
    let baseline = cur();
    let baseline_peak = pk();

    for _ in 0..500 {
        harness.run();
    }

    let post = cur();
    let post_peak = pk();
    let added_peak = post_peak.saturating_sub(baseline_peak);
    let leaked = post.saturating_sub(baseline);
    eprintln!(
        "workbench heap_ceiling: baseline={baseline} bytes, post={post} bytes, \
         baseline_peak={baseline_peak} bytes, post_peak={post_peak} bytes, \
         added_peak={added_peak} bytes, leaked={leaked} bytes, frames=500",
    );

    assert!(
        added_peak < STEADY_STATE_CEILING_BYTES,
        "workbench peak heap grew by {added_peak} bytes across 500 steady-state \
         frames, ceiling is {STEADY_STATE_CEILING_BYTES}. A per-frame \
         allocation that isn't being released, or an unbounded internal cache.",
    );
}
