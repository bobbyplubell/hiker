//! Stable byte positions that survive edits via Set::map_pos.

use crate::change::Set;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Bias {
    /// Stay on the left side of an insertion at this position.
    Left,
    /// Stay on the right side of an insertion at this position.
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Anchor {
    pub byte: u32,
    pub bias: Bias,
}

impl Anchor {
    pub const fn at(byte: usize, bias: Bias) -> Self {
        Self { byte: byte as u32, bias }
    }

    pub const fn offset(&self) -> usize {
        self.byte as usize
    }

    pub fn map(self, changes: &Set) -> Self {
        Self {
            byte: changes.map_pos(self.byte as usize, self.bias) as u32,
            bias: self.bias,
        }
    }
}
