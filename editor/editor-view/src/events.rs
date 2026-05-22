//! Backend-neutral input events. Host backends translate native events
//! (egui::Event, winit::WindowEvent, ...) into [`InputEvent`].

use smol_str::SmolStr;

#[derive(Clone, Debug)]
pub enum InputEvent {
    Key(KeyEvent),
    Text(SmolStr),
    Ime(ImeEvent),
    Mouse(MouseEvent),
    Scroll { delta_x: f32, delta_y: f32 },
    Focus(bool),
    Paste(String),
    Copy,
    Cut,
}

#[derive(Clone, Copy, Debug)]
pub struct KeyEvent {
    pub key: Key,
    pub mods: Modifiers,
    pub repeat: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Named(NamedKey),
    Char(char),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NamedKey {
    Enter,
    Tab,
    Escape,
    Backspace,
    Delete,
    Space,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Cmd on macOS, Win on Windows, Super on Linux. Use the OS's "primary"
    /// modifier — egui already collapses this onto `command` in its events.
    pub meta: bool,
}

impl Modifiers {
    pub const fn is_empty(&self) -> bool {
        !self.ctrl && !self.alt && !self.shift && !self.meta
    }

    /// True if `mods` matches exactly the same primary modifier set, ignoring shift.
    pub const fn primary_only(&self) -> bool {
        (self.ctrl ^ self.meta) && !self.alt && !self.shift
    }

    pub const fn primary(&self) -> bool {
        self.ctrl || self.meta
    }
}

#[derive(Clone, Copy, Debug)]
pub enum MouseEvent {
    Down { button: MouseButton, x: f32, y: f32, click_count: u8 },
    Up { button: MouseButton, x: f32, y: f32 },
    Move { x: f32, y: f32 },
    Drag { x: f32, y: f32, button: MouseButton },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Debug)]
pub enum ImeEvent {
    Enabled,
    Disabled,
    Preedit(SmolStr),
    Commit(SmolStr),
}
