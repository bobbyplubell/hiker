//! Two-finger trackpad swipe → back/forward navigation, browser-style.
//!
//! Lives alongside `swipe_indicator` because the two are halves of the
//! same gesture: this module accumulates horizontal scroll deltas and
//! commits a nav action when the user crosses the threshold; the
//! indicator widget draws the on-screen progress affordance using the
//! same accumulator/armed-direction state.
//!
//! Originally inlined in `keybinds.rs`, but it's not really a "keybind"
//! — it's a gesture handler that happens to live near the keyboard input
//! pump. Extracted so the keybinds module stays focused on keyboard
//! chords and the swipe machinery is one click away from its widget.

use eframe::egui;

use crate::editor_pane;
use crate::state::AppState;

/// Two-finger trackpad swipe (horizontal scroll) navigates the nav stack
/// like a browser. We accumulate horizontal scroll delta until it crosses
/// a threshold, then fire `nav_go` and start a short cooldown so the rest
/// of the gesture doesn't fire repeatedly.
///
/// Gates:
/// - horizontal-dominant: `|dx| > 1.5 * |dy|` (avoids triggering during a
///   mostly-vertical scroll with a stray x component)
/// - no TextEdit / editor focus (we don't want to steal scroll from a
///   focused input)
/// - hovered widget isn't a horizontal ScrollArea (those want the scroll)
impl AppState {
    pub fn handle_swipe_nav(&mut self, ctx: &egui::Context) {
    let state = self;
    // Opt-out: `[ui].swipe_nav_enabled` (default true). When off, two-finger
    // horizontal scroll never navigates. [navigation-swipe-disable]
    if !state
        .vault_session
        .config
        .read()
        .map(|c| c.ui.swipe_nav_enabled)
        .unwrap_or(true)
    {
        state.session.nav.swipe_accum_x = 0.0;
        return;
    }
    let now = std::time::Instant::now();
    // egui delivers scroll inputs from the platform; on macOS/Windows
    // touchpads a two-finger horizontal swipe lands here as x-axis delta.
    let (dx, dy) = ctx.input(|i| (i.smooth_scroll_delta.x, i.smooth_scroll_delta.y));

    // Cooldown: gesture-triggered nav locks for ~350ms so a single
    // continuous swipe doesn't fire multiple times. Indicator overlay
    // keeps drawing during this window so the user sees the "committed"
    // flash. Accumulator is held at the threshold (sign preserved) so
    // the overlay paints at full progress.
    if let Some(until) = state.session.nav.swipe_cooldown_until
        && now < until
    {
        // request a repaint so the overlay animates smoothly even
        // without further input events.
        ctx.request_repaint();
        return;
    }
    // Past cooldown end: clear it + drop the committed direction.
    if let Some(until) = state.session.nav.swipe_cooldown_until
        && now >= until
    {
        state.session.nav.swipe_cooldown_until = None;
        state.session.nav.swipe_last_commit_dir = None;
        state.session.nav.swipe_accum_x = 0.0;
    }

    // Ignore if a text-edit has focus — the user might be horizontally
    // scrolling inside the input.
    let has_text_focus = ctx.memory(|m| m.focused().is_some());
    if has_text_focus {
        state.session.nav.swipe_accum_x = 0.0;
        state.session.nav.swipe_last_activity = None;
        return;
    }

    // Skip when the pointer is over a widget that owns horizontal
    // scroll (editor body, tab strip, horizontal code blocks). The
    // widget registers its rect into `swipe_skip_rects` during render.
    // We still process the cooldown / decay paths above this check so
    // an already-armed gesture continues to commit / fade cleanly.
    if let Some(pos) = ctx.pointer_hover_pos()
        && state.session.nav.swipe_skip_rects.iter().any(|r| r.contains(pos))
    {
        // The user is scrolling inside content, not navigating. Reset
        // any partial accumulator so we don't carry forward a half-built
        // swipe across rect boundaries.
        if state.session.nav.swipe_armed_dir.is_none() {
            state.session.nav.swipe_accum_x = 0.0;
            state.session.nav.swipe_last_activity = None;
        }
        return;
    }

    const THRESHOLD: f32 = 120.0;

    // No-input frame (fingers lifted / momentum done): just decay the indicator
    // smoothly toward 0. We do NOT commit here — committing on the threshold
    // crossing below means we never wait on the touchpad's momentum-scroll tail,
    // which is what made the old release-time commit feel laggy / hung.
    let no_input = dx.abs() < 0.01 && dy.abs() < 0.01;
    if no_input {
        if state.session.nav.swipe_accum_x.abs() > 0.5 {
            state.session.nav.swipe_accum_x *= 0.8;
            ctx.request_repaint();
        } else {
            state.session.nav.swipe_accum_x = 0.0;
            state.session.nav.swipe_last_activity = None;
        }
        return;
    }

    // Horizontal-dominant only.
    if dx.abs() <= dy.abs() * 1.5 {
        if dy.abs() > dx.abs() {
            // Vertical-dominant scroll cancels a partial swipe.
            state.session.nav.swipe_accum_x = 0.0;
        }
        return;
    }

    state.session.nav.swipe_accum_x += dx;
    state.session.nav.swipe_last_activity = Some(now);

    // Commit the instant the accumulator crosses the threshold — responsive,
    // with no wait for the gesture to be "released". A short cooldown then keeps
    // a single continuous swipe (incl. its momentum tail) from firing again.
    let accum = state.session.nav.swipe_accum_x;
    if accum.abs() >= THRESHOLD {
        // Positive dx (swipe right) → back; negative → forward.
        let dir: i8 = if accum > 0.0 { -1 } else { 1 };
        editor_pane::nav_go(state, dir as i32);
        state.session.nav.swipe_last_commit_dir = Some(dir);
        // Hold the accumulator at full (sign preserved) so the indicator flashes
        // saturated during the cooldown.
        state.session.nav.swipe_accum_x = (dir as f32) * -THRESHOLD;
        state.session.nav.swipe_cooldown_until =
            Some(now + std::time::Duration::from_millis(350));
    }
    ctx.request_repaint();
    }
}
