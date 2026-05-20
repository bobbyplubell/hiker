//! Translate egui events into [`InputEvent`].

use editor_view::{ImeEvent, InputEvent, Key, KeyEvent, Modifiers, MouseButton, MouseEvent, NamedKey};
use egui::Event as Ev;
use smol_str::SmolStr;

pub fn translate(ev: &Ev) -> Option<InputEvent> {
    match ev {
        Ev::Text(s) => Some(InputEvent::Text(SmolStr::from(s))),
        Ev::Key { key, pressed: true, modifiers, repeat, .. } => {
            let mods = translate_mods(modifiers);
            let k = translate_key(*key)?;
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

fn translate_mods(m: &egui::Modifiers) -> Modifiers {
    Modifiers {
        ctrl: m.ctrl,
        alt: m.alt,
        shift: m.shift,
        // egui collapses cmd/win onto `mac_cmd` on mac, `ctrl` elsewhere; `command`
        // is the cross-platform primary. Keep them split for downstream commands
        // that care.
        meta: m.mac_cmd,
    }
}

fn translate_key(k: egui::Key) -> Option<Key> {
    use egui::Key as K;
    let named = match k {
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
    let (pressed, released, down, double, triple) = ctx.input(|i| {
        (
            i.pointer.primary_pressed(),
            i.pointer.primary_released(),
            i.pointer.primary_down(),
            i.pointer.button_double_clicked(egui::PointerButton::Primary),
            i.pointer.button_triple_clicked(egui::PointerButton::Primary),
        )
    });
    let over = response.contains_pointer();

    if pressed && over {
        let cc = if triple { 3 } else if double { 2 } else { 1 };
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
