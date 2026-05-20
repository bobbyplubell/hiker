//! IME state machine: tracks the currently-composing preedit text so the view
//! can render it as a phantom decoration at the selection head.

use smol_str::SmolStr;

#[derive(Default, Clone, Debug)]
pub struct ImeState {
    /// Current preedit (uncommitted) text, anchored at the main selection head
    /// at the time the preedit was last updated.
    pub preedit: Option<SmolStr>,
    pub enabled: bool,
}

impl ImeState {
    pub fn clear_preedit(&mut self) {
        self.preedit = None;
    }
}
