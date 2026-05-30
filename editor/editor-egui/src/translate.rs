//! Translate egui events into [`InputEvent`].

use editor_view::events::ImeEvent;

use editor_view::events::InputEvent;

use editor_view::events::Key;

use editor_view::events::KeyEvent;

use editor_view::events::Modifiers;

use editor_view::events::MouseButton;

use editor_view::events::MouseEvent;

use editor_view::events::NamedKey;
use egui::Event as Ev;
use smol_str::SmolStr;

pub fn translate(ev: &Ev) -> Option<InputEvent> {
    match ev {
        Ev::Text(s) => Some(InputEvent::Text(SmolStr::from(s))),
        Ev::Key { key, pressed: true, modifiers: m, repeat, .. } => {
            // egui collapses cmd/win onto `mac_cmd` on mac, `ctrl` elsewhere;
            // `command` is the cross-platform primary. Keep them split for
            // downstream commands that care.
            let mods = Modifiers { ctrl: m.ctrl, alt: m.alt, shift: m.shift, meta: m.mac_cmd };
            let k = EguiKey(*key).into_named()?;
            Some(InputEvent::Key(KeyEvent { key: k, mods, repeat: *repeat }))
        }
        Ev::Ime(ime) => Some(InputEvent::Ime(match ime {
            egui::ImeEvent::Enabled => ImeEvent::Enabled,
            egui::ImeEvent::Disabled => ImeEvent::Disabled,
            egui::ImeEvent::Preedit(s) => ImeEvent::Preedit(SmolStr::from(s)),
            egui::ImeEvent::Commit(s) => ImeEvent::Commit(SmolStr::from(s)),
        })),
        Ev::Copy => Some(InputEvent::Copy),
        Ev::Cut => Some(InputEvent::Cut),
        Ev::Paste(s) => Some(InputEvent::Paste(s.clone())),
        // Raw MouseWheel events are intentionally NOT translated here. The
        // widget reads `smooth_scroll_delta` directly each frame so we get
        // egui's accumulated, momentum-smoothed scroll regardless of focus.
        // Translating raw deltas resulted in tick-sized scrolls (very slow)
        // when focused but smooth scrolls when only hovered — confusing.
        Ev::MouseWheel { .. } => None,
        _ => None,
    }
}

/// Newtype wrapper so the big egui-key → internal-key mapping can live in
/// a `self`-method (and not be flagged as single-use). Inlining the match
/// into `translate` would dominate that function's body.
struct EguiKey(egui::Key);

impl EguiKey {
    const fn into_named(self) -> Option<Key> {
        use egui::Key as K;
        let named = match self.0 {
        K::Enter => NamedKey::Enter,
        K::Tab => NamedKey::Tab,
        K::Escape => NamedKey::Escape,
        K::Backspace => NamedKey::Backspace,
        K::Delete => NamedKey::Delete,
        K::Space => NamedKey::Space,
        K::ArrowLeft => NamedKey::ArrowLeft,
        K::ArrowRight => NamedKey::ArrowRight,
        K::ArrowUp => NamedKey::ArrowUp,
        K::ArrowDown => NamedKey::ArrowDown,
        K::Home => NamedKey::Home,
        K::End => NamedKey::End,
        K::PageUp => NamedKey::PageUp,
        K::PageDown => NamedKey::PageDown,
        K::A => return Some(Key::Char('a')),
        K::B => return Some(Key::Char('b')),
        K::C => return Some(Key::Char('c')),
        K::D => return Some(Key::Char('d')),
        K::E => return Some(Key::Char('e')),
        K::F => return Some(Key::Char('f')),
        K::G => return Some(Key::Char('g')),
        K::H => return Some(Key::Char('h')),
        K::I => return Some(Key::Char('i')),
        K::J => return Some(Key::Char('j')),
        K::K => return Some(Key::Char('k')),
        K::L => return Some(Key::Char('l')),
        K::M => return Some(Key::Char('m')),
        K::N => return Some(Key::Char('n')),
        K::O => return Some(Key::Char('o')),
        K::P => return Some(Key::Char('p')),
        K::Q => return Some(Key::Char('q')),
        K::R => return Some(Key::Char('r')),
        K::S => return Some(Key::Char('s')),
        K::T => return Some(Key::Char('t')),
        K::U => return Some(Key::Char('u')),
        K::V => return Some(Key::Char('v')),
        K::W => return Some(Key::Char('w')),
        K::X => return Some(Key::Char('x')),
        K::Y => return Some(Key::Char('y')),
        K::Z => return Some(Key::Char('z')),
        _ => return None,
    };
    Some(Key::Named(named))
    }
}

pub fn pointer_mouse_events(
    ctx: &egui::Context,
    response: &egui::Response,
    rect: egui::Rect,
    has_active_drag: bool,
) -> Vec<MouseEvent> {
    let mut out = Vec::new();
    let Some(pos) = ctx.pointer_interact_pos() else {
        return out;
    };
    let local = pos - rect.min;
    let (x, y) = (local.x, local.y);

    // Press-frame Down. We emit the moment the primary button is pressed
    // and the pointer is over our widget, NOT when egui later says the
    // click was completed or a drag was detected. `clicked()` only fires
    // on release after a short press, and `drag_started_by()` only fires
    // after a small motion deadzone — both would make the caret lag /
    // jump.
    let (pressed, released, down) = ctx.input(|i| {
        (
            i.pointer.primary_pressed(),
            i.pointer.primary_released(),
            i.pointer.primary_down(),
        )
    });
    let over = response.contains_pointer();

    if pressed && over {
        // egui only reports double/triple clicks on the RELEASE frame, but we
        // emit `Down` (which carries the click count that drives word/line
        // selection) on the PRESS frame — so reading egui's flags here always
        // saw `1`. Count successive presses ourselves, keyed per widget in
        // egui temp memory: presses close in time and space chain 1→2→3.
        let now = ctx.input(|i| i.time);
        let id = response.id.with("editor-multiclick");
        let prev = ctx.data(|d| d.get_temp::<ClickTracker>(id));
        let cc = next_click_count(prev, now, x, y);
        ctx.data_mut(|d| {
            d.insert_temp(id, ClickTracker { last_time: now, last_x: x, last_y: y, count: cc });
        });
        out.push(MouseEvent::Down { button: MouseButton::Left, x, y, click_count: cc });
    }
    // Emit Drag every frame the button is held AND we know we have an
    // active interaction we initiated (i.e. the host's drag state machine
    // is not Idle). Relying on `response.dragged()` or
    // `is_pointer_button_down_on()` was unreliable here: the former
    // requires crossing a ~6 px deadzone (so tiny selection drags never
    // started), and the latter can be false in nested-Ui scenarios where
    // egui's interaction system attributes the press to a different
    // widget id than ours.
    if down && !pressed && has_active_drag {
        out.push(MouseEvent::Drag { x, y, button: MouseButton::Left });
    }
    if released && has_active_drag {
        out.push(MouseEvent::Up { button: MouseButton::Left, x, y });
    }
    out
}

/// Per-widget press-time multi-click state, stashed in egui temp memory. Lets
/// us derive the click count when emitting `Down` on the press frame, since
/// egui's own double/triple-click flags only go true on release.
#[derive(Clone, Copy)]
struct ClickTracker {
    last_time: f64,
    last_x: f32,
    last_y: f32,
    count: u8,
}

/// Max seconds between presses for them to chain into a multi-click, and max
/// pointer travel (px) allowed between them. Mirror egui's own click
/// thresholds closely enough that the feel matches.
const MULTICLICK_DELAY: f64 = 0.3;
const MULTICLICK_DIST: f32 = 6.0;

/// Click count for a press at (`now`, `x`, `y`) given the previous press.
/// Chains 1→2→3 then wraps back to 1 when presses are close in time and space;
/// resets to 1 when the gap is too long or the pointer moved too far.
fn next_click_count(prev: Option<ClickTracker>, now: f64, x: f32, y: f32) -> u8 {
    match prev {
        Some(p)
            if now - p.last_time <= MULTICLICK_DELAY
                && (x - p.last_x).abs() <= MULTICLICK_DIST
                && (y - p.last_y).abs() <= MULTICLICK_DIST =>
        {
            if p.count >= 3 { 1 } else { p.count + 1 }
        }
        _ => 1,
    }
}

#[cfg(test)]
mod multiclick_tests {
    use super::{next_click_count, ClickTracker};

    fn tracker(count: u8, last_time: f64) -> ClickTracker {
        ClickTracker { last_time, last_x: 10.0, last_y: 10.0, count }
    }

    #[test]
    fn first_press_is_single() {
        assert_eq!(next_click_count(None, 1.0, 10.0, 10.0), 1);
    }

    #[test]
    fn rapid_presses_chain_then_wrap() {
        // 1 → 2 → 3 → 1 when each press is within the delay + distance.
        let c1 = next_click_count(None, 1.00, 10.0, 10.0);
        assert_eq!(c1, 1);
        let c2 = next_click_count(Some(tracker(c1, 1.00)), 1.10, 11.0, 10.0);
        assert_eq!(c2, 2);
        let c3 = next_click_count(Some(tracker(c2, 1.10)), 1.20, 10.0, 11.0);
        assert_eq!(c3, 3);
        let c4 = next_click_count(Some(tracker(c3, 1.20)), 1.30, 10.0, 10.0);
        assert_eq!(c4, 1, "fourth rapid press wraps back to single");
    }

    #[test]
    fn slow_second_press_resets_to_single() {
        assert_eq!(next_click_count(Some(tracker(1, 1.0)), 1.0 + 0.5, 10.0, 10.0), 1);
    }

    #[test]
    fn moved_far_resets_to_single() {
        assert_eq!(next_click_count(Some(tracker(1, 1.0)), 1.05, 40.0, 10.0), 1);
    }
}
