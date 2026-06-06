//! Thin wrapper over the `puffin` profiler.
//!
//! Gated behind the `profiling` cargo feature. When the feature is off,
//! every macro and function in here compiles to a no-op so the release
//! build pays nothing. When it's on:
//!
//! - [`init_server`] starts a `puffin_http` server on 127.0.0.1:8585 at
//!   app boot. Connect with the standalone `puffin_viewer` GUI
//!   (`cargo install puffin_viewer`, then `puffin_viewer --url 127.0.0.1:8585`).
//! - The frame loop calls [`new_frame`] once per egui frame so puffin
//!   knows where to slice the timeline.
//! - Hot paths sprinkle `profile_function!()` or `profile_scope!("…")`
//!   markers, which `puffin` records as nested spans.
//! - F12 toggles collection (the server stays up either way, but
//!   `set_scopes_on` flips whether new scopes get recorded).
//!
//! Add new markers freely — they're zero-cost when the feature is off,
//! and they're the only way to see where a frame is actually spending
//! its time. Don't try to "guess" without them.
//!
//! Run with `cargo run --features profiling` (debug) or
//! `cargo run --release --features profiling`.

/// Hold the puffin_http server alive for the process lifetime when the
/// `profiling` feature is on. Dropping the handle stops accepting new
/// viewer connections. Stored on a static so callers don't have to
/// thread it through `AppState`.
#[cfg(feature = "profiling")]
static PUFFIN_SERVER: std::sync::OnceLock<puffin_http::Server> = std::sync::OnceLock::new();

/// In-process `FrameView` that mirrors every recorded frame. Fed by a
/// global `FrameSink` registered in [`init_server`]. We keep a copy on
/// our side (in addition to `puffin_http`'s in-memory ring) so [`capture_to_file`]
/// can dump a snapshot to disk on demand without involving the viewer.
#[cfg(feature = "profiling")]
static FRAME_VIEW: std::sync::OnceLock<std::sync::Mutex<puffin::FrameView>> =
    std::sync::OnceLock::new();

/// Zero-sized handle for the boot-time / per-frame profiler hooks. Kept
/// as inherent methods (rather than free fns) so the single boot / frame
/// call sites don't trip `single_call_fn`.
pub struct Profiler;

impl Profiler {
/// Start the puffin_http server + the in-process frame mirror. Call
/// once at boot. Idempotent. No-op without the `profiling` feature.
#[cfg(not(feature = "profiling"))]
pub const fn init_server(self) {}

/// Start the puffin_http server + the in-process frame mirror. Call
/// once at boot. Idempotent.
#[cfg(feature = "profiling")]
pub fn init_server(self) {
    {
        // FrameView mirror: every recorded frame is also pushed here so
        // `capture_to_file` can dump a snapshot without involving the
        // external viewer. Registered as a global sink on the puffin
        // GlobalProfiler.
        let _ = FRAME_VIEW.get_or_init(|| std::sync::Mutex::new(puffin::FrameView::default()));
        puffin::GlobalProfiler::lock().add_sink(Box::new(|frame| {
            if let Some(view) = FRAME_VIEW.get()
                && let Ok(mut g) = view.lock()
            {
                let _ = g.add_frame(frame);
            }
        }));

        let _ = PUFFIN_SERVER.get_or_init(|| {
            let bind = "127.0.0.1:8585";
            match puffin_http::Server::new(bind) {
                Ok(server) => {
                    eprintln!(
                        "puffin: profiler server on {bind} — connect with \
                         `puffin_viewer --url {bind}`",
                    );
                    // Default to collection-on so frames record immediately.
                    puffin::set_scopes_on(true);
                    server
                }
                Err(err) => {
                    eprintln!("puffin: failed to bind {bind}: {err}");
                    // Construct an unusable shim that drops cleanly. We
                    // can't easily express "no server" with OnceLock; the
                    // simplest workable path is to retry binding next
                    // boot. Panicking here would be hostile.
                    puffin_http::Server::new("127.0.0.1:0").expect("any-port bind")
                }
            }
        });
    }
}
}

/// Write the captured frames to disk as both a `.puffin` binary (for
/// the external viewer) and a `.txt` text summary (for code review).
/// Returns the paths written, or an error string for the UI to toast.
///
/// The text summary aggregates every scope by name across all captured
/// frames and reports count + total / mean / p50 / p95 / max duration.
/// That's the actually-useful data for spotting hotspots; the binary
/// `.puffin` is the round-trippable one if you want to drill in via the
/// viewer later.
#[cfg(feature = "profiling")]
pub fn capture_to_file(dir: &std::path::Path) -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    let Some(view_lock) = FRAME_VIEW.get() else {
        return Err("profiling not initialised".into());
    };
    let view = view_lock.lock().map_err(|_| "frame-view lock poisoned")?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let bin_path = dir.join(format!("profile-{stamp}.puffin"));
    let txt_path = dir.join(format!("profile-{stamp}.txt"));

    // Binary dump — what `puffin_viewer profile-<stamp>.puffin` opens.
    {
        let f = std::fs::File::create(&bin_path)
            .map_err(|e| format!("create {}: {e}", bin_path.display()))?;
        let mut w = std::io::BufWriter::new(f);
        view.write(&mut w)
            .map_err(|e| format!("save .puffin: {e}"))?;
    }

    // Text summary — aggregated scope timings, ready for code review.
    let summary = summarize_frames(&view);
    std::fs::write(&txt_path, summary)
        .map_err(|e| format!("write {}: {e}", txt_path.display()))?;

    Ok((bin_path, txt_path))
}

/// Walk every captured frame and aggregate scope durations using
/// puffin's built-in merge. Prints a flat sorted list (by total time)
/// of every scope across every thread, plus a frame-level p50 / p95 /
/// max so a few catastrophic frames don't hide behind the average.
#[cfg(feature = "profiling")]
fn summarize_frames(view: &puffin::FrameView) -> String {
    use std::collections::BTreeSet;
    use std::fmt::Write;

    let fmt_ms = |ns: i64| format!("{:.3} ms", ns as f64 / 1_000_000.0);
    let fmt_us = |ns: i64| format!("{:>9.1} us", ns as f64 / 1_000.0);

    let mut out = String::new();
    let _ = writeln!(out, "# Puffin capture summary");

    // Unpack every frame so the merge can walk thread streams. Some
    // frames may still be in packed form on disk; this returns Arcs.
    let unpacked: Vec<std::sync::Arc<puffin::UnpackedFrameData>> = view
        .recent_frames()
        .filter_map(|fd| fd.unpacked().ok())
        .collect();
    let frame_count = unpacked.len();
    if frame_count == 0 {
        let _ = writeln!(out, "# (no frames captured)");
        return out;
    }

    // Frame-time stats (max - min across each frame's range_ns).
    let mut frame_durations: Vec<i64> = unpacked
        .iter()
        .map(|f| {
            let (lo, hi) = f.range_ns();
            hi - lo
        })
        .collect();
    frame_durations.sort_unstable();
    let frame_total: i64 = frame_durations.iter().sum();
    let frame_mean = frame_total / frame_count as i64;
    let frame_p50 = frame_durations[frame_durations.len() / 2];
    let p95_idx = (((frame_durations.len() as f64) * 0.95) as usize)
        .min(frame_durations.len() - 1);
    let frame_p95 = frame_durations[p95_idx];
    let frame_max = *frame_durations.last().unwrap();
    let _ = writeln!(out, "# Frames captured: {frame_count}");
    let _ = writeln!(
        out,
        "# Frame time:  mean={}  p50={}  p95={}  max={}",
        fmt_ms(frame_mean),
        fmt_ms(frame_p50),
        fmt_ms(frame_p95),
        fmt_ms(frame_max),
    );

    // Collect the union of every thread that appeared in any frame so
    // we can merge per thread and aggregate into one big list.
    let mut threads: BTreeSet<&puffin::ThreadInfo> = BTreeSet::new();
    for f in &unpacked {
        for k in f.thread_streams.keys() {
            threads.insert(k);
        }
    }

    let scope_collection = view.scope_collection();

    // Flatten merged scopes into a single (name, total, max, frames-touched) list.
    struct FlatRow {
        name: String,
        total_ns: i64,
        mean_per_frame_ns: i64,
        max_ns: i64,
        num_pieces: usize,
    }
    let mut rows: Vec<FlatRow> = Vec::new();

    fn walk<'s>(
        out: &mut Vec<FlatRow>,
        scope_collection: &puffin::ScopeCollection,
        prefix: &str,
        scopes: &[puffin::MergeScope<'s>],
    ) {
        for s in scopes {
            let name = scope_collection
                .fetch_by_id(&s.id)
                .map(|d| {
                    d.scope_name
                        .as_ref()
                        .map(|n| n.as_ref().to_string())
                        .unwrap_or_else(|| d.function_name.as_ref().to_string())
                })
                .unwrap_or_else(|| "<anon>".to_string());
            let display = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix} > {name}")
            };
            out.push(FlatRow {
                name: display.clone(),
                total_ns: s.total_duration_ns,
                mean_per_frame_ns: s.duration_per_frame_ns,
                max_ns: s.max_duration_ns,
                num_pieces: s.num_pieces,
            });
            // Recurse into children. Keep two levels of nesting visible;
            // beyond that the names become noisy.
            if prefix.matches(" > ").count() < 2 {
                walk(out, scope_collection, &display, &s.children);
            }
        }
    }

    for thread in threads {
        let Ok(merged) = puffin::merge_scopes_for_thread(scope_collection, &unpacked, thread)
        else {
            continue;
        };
        let prefix = format!("[{}]", thread.name);
        walk(&mut rows, scope_collection, &prefix, &merged);
    }

    rows.sort_by(|a, b| b.total_ns.cmp(&a.total_ns));

    let _ = writeln!(out, "#");
    let _ = writeln!(
        out,
        "# Scope                                              pieces      total/frame         max          total",
    );
    let _ = writeln!(
        out,
        "# -------------------------------------------------- ------ ---------------- ------------- ----------------",
    );
    for r in rows.iter().take(100) {
        let trimmed: String = if r.name.len() > 50 {
            format!("{}…", &r.name[..49])
        } else {
            r.name.clone()
        };
        let _ = writeln!(
            out,
            "  {:<50}  {:>6}  {}  {}  {}",
            trimmed,
            r.num_pieces,
            fmt_us(r.mean_per_frame_ns),
            fmt_us(r.max_ns),
            fmt_us(r.total_ns),
        );
    }
    out
}

impl Profiler {
/// Mark the start of a new frame. Call once at the top of `update`.
/// No-op without the `profiling` feature.
#[cfg(not(feature = "profiling"))]
pub const fn new_frame(self) {}

/// Mark the start of a new frame. Call once at the top of `update`.
#[cfg(feature = "profiling")]
pub fn new_frame(self) {
    puffin::GlobalProfiler::lock().new_frame();
}
}

/// Enable / disable the global profiler. Toggling off stops collection;
/// connected viewers freeze on the last captured frames until it's
/// re-enabled.
#[cfg(feature = "profiling")]
pub fn set_enabled(on: bool) {
    puffin::set_scopes_on(on);
}

/// Whether profiling collection is currently enabled.
#[cfg(feature = "profiling")]
pub fn is_enabled() -> bool {
    puffin::are_scopes_on()
}

#[cfg(not(feature = "profiling"))]
#[inline(always)]
pub const fn set_enabled(_on: bool) {}

#[cfg(not(feature = "profiling"))]
#[inline(always)]
#[allow(dead_code)]
pub const fn is_enabled() -> bool {
    false
}

/// Mark the enclosing function as a profile span. No-op without the
/// `profiling` feature.
#[macro_export]
macro_rules! profile_function {
    () => {
        #[cfg(feature = "profiling")]
        puffin::profile_function!();
    };
    ($extra:expr) => {
        #[cfg(feature = "profiling")]
        puffin::profile_function!($extra);
    };
}

/// Mark an explicit scope. Useful inside long functions or inside loop
/// bodies where you want a tighter span than the whole function.
#[macro_export]
macro_rules! profile_scope {
    ($name:expr) => {
        #[cfg(feature = "profiling")]
        puffin::profile_scope!($name);
    };
    ($name:expr, $extra:expr) => {
        #[cfg(feature = "profiling")]
        puffin::profile_scope!($name, $extra);
    };
}

// ---------------------------------------------------------------------------
// Lightweight always-on frame profiler — independent of the `profiling`
// feature and of `puffin_viewer`, so a plain `cargo run --release` can measure
// the real per-frame cost and print a section breakdown to stderr.
//
// Env-gated (read once; zero overhead when off):
//   HIKER_FRAMELOG=1   print wall-time stats + per-section breakdown each ~180 frames
//   HIKER_BENCH=1      also force continuous repaint, to simulate a scroll/redraw load
//
// Usage: call `FrameProf::tick(ctx)` once at the top of `update()`, and wrap
// hot sections with `let _g = FrameProf::guard("name");`.
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicU8, Ordering};

static FP_STATE: AtomicU8 = AtomicU8::new(0); // 0 = uninit, 1 = off, 2 = on

/// Cheap enabled check; resolves the env vars exactly once.
fn fp_enabled() -> bool {
    match FP_STATE.load(Ordering::Relaxed) {
        2 => true,
        1 => false,
        _ => {
            let on = std::env::var_os("HIKER_FRAMELOG").is_some()
                || std::env::var_os("HIKER_BENCH").is_some();
            FP_STATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

fn fp_bench() -> bool {
    std::env::var_os("HIKER_BENCH").is_some()
}

/// A frame slower than this (ms) is logged individually with its breakdown —
/// at 60Hz vsync the budget is 16.67ms, so >24ms means a missed vblank / stutter.
const SPIKE_MS: f32 = 24.0;

struct FrameProfState {
    last: Option<std::time::Instant>,
    dts: Vec<f32>, // ms
    /// Section ms for the frame currently being built (reset every frame).
    cur: std::collections::HashMap<&'static str, f64>,
    /// Rolling per-section sums for the periodic report.
    roll: std::collections::HashMap<&'static str, (f64, u32)>,
}

fn fp_state() -> &'static std::sync::Mutex<FrameProfState> {
    static S: std::sync::OnceLock<std::sync::Mutex<FrameProfState>> = std::sync::OnceLock::new();
    S.get_or_init(|| {
        std::sync::Mutex::new(FrameProfState {
            last: None,
            dts: Vec::new(),
            cur: std::collections::HashMap::new(),
            roll: std::collections::HashMap::new(),
        })
    })
}

/// Zero-sized handle for the frame profiler (methods, not free fns, to match
/// the `Profiler` style and avoid `single_call_fn` churn).
pub struct FrameProf;

impl FrameProf {
    /// Record the inter-frame wall time; print a report every ~180 frames.
    /// In `HIKER_BENCH` mode also drives continuous repaint.
    pub fn tick(ctx: &egui::Context) {
        if !fp_enabled() {
            return;
        }
        if fp_bench() {
            ctx.request_repaint();
        }
        let mut s = fp_state().lock().unwrap();
        let now = std::time::Instant::now();
        if let Some(prev) = s.last {
            let dt = (now - prev).as_secs_f32() * 1e3;
            s.dts.push(dt);
            // Finalize the frame that just ended: fold its section times into the
            // rolling report, and if it was a stutter, print its breakdown now.
            let cur = std::mem::take(&mut s.cur);
            if dt > SPIKE_MS {
                let mut secs: Vec<_> = cur.iter().map(|(k, v)| (*k, *v)).collect();
                secs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
                let top: String = secs
                    .iter()
                    .take(5)
                    .map(|(k, v)| format!("{k} {v:.1}ms"))
                    .collect::<Vec<_>>()
                    .join(", ");
                eprintln!("[frameprof] SPIKE {dt:.1}ms  | {top}");
            }
            for (k, v) in cur {
                let e = s.roll.entry(k).or_insert((0.0, 0));
                e.0 += v;
                e.1 += 1;
            }
        }
        s.last = Some(now);
        if s.dts.len() >= 180 {
            let mut d = std::mem::take(&mut s.dts);
            let sections = std::mem::take(&mut s.roll);
            drop(s);
            d.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let n = d.len();
            let mean = d.iter().sum::<f32>() / n as f32;
            let p = |q: f32| d[((n as f32 * q) as usize).min(n - 1)];
            eprintln!(
                "\n[frameprof] {n} frames | wall mean {:.2}ms p50 {:.2} p99 {:.2} max {:.2} => {:.0} fps",
                mean, p(0.50), p(0.99), p(1.0), 1000.0 / mean.max(0.001)
            );
            let mut secs: Vec<_> = sections
                .into_iter()
                .map(|(k, (sum, c))| (k, sum / c.max(1) as f64, c))
                .collect();
            secs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            for (name, meanms, c) in secs.into_iter().take(12) {
                eprintln!("[frameprof]   {meanms:>7.2}ms  x{c:<4} {name}");
            }
        }
    }

    /// Time a named section; the returned guard records on drop. Returns `None`
    /// (and costs nothing) when the frame profiler is disabled.
    pub fn guard(name: &'static str) -> Option<SectionGuard> {
        if fp_enabled() {
            Some(SectionGuard { name, start: std::time::Instant::now() })
        } else {
            None
        }
    }
}

pub struct SectionGuard {
    name: &'static str,
    start: std::time::Instant,
}

impl Drop for SectionGuard {
    fn drop(&mut self) {
        let ms = self.start.elapsed().as_secs_f64() * 1e3;
        if let Ok(mut s) = fp_state().lock() {
            *s.cur.entry(self.name).or_insert(0.0) += ms;
        }
    }
}
