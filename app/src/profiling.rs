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
/// once at boot. Idempotent.
pub const fn init_server(self) {
    #[cfg(feature = "profiling")]
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
pub const fn new_frame(self) {
    #[cfg(feature = "profiling")]
    {
        puffin::GlobalProfiler::lock().new_frame();
    }
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
