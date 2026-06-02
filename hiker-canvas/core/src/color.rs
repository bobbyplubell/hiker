//! JSON Canvas color values. A color on the wire is a bare JSON string that is
//! EITHER a preset slot (`"1".."6"`, mapped to theme tokens by a later render
//! layer) OR a `#RRGGBB` hex literal. This module models both as one [`Color`]
//! enum and (de)serializes it transparently as that bare string, so the core
//! stores slot numbers without hard-coding any RGB.
//
// status: canvas-color-model

use serde::de::{Error as DeError, Unexpected};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A JSON Canvas color: a preset slot or a hex literal.
///
/// Serializes as a bare JSON string (`"4"` or `"#ff8800"`), matching the
/// on-the-wire format exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Color {
    /// A preset slot in `1..=6`. The actual RGB is resolved by the render
    /// layer through the active theme, never here.
    Preset(u8),
    /// A literal `#RRGGBB` hex string (stored with its leading `#`).
    Hex(String),
}

impl Color {
    /// Render the color back to its on-the-wire string form.
    #[must_use]
    pub fn as_wire_string(&self) -> String {
        match self {
            Self::Preset(slot) => slot.to_string(),
            Self::Hex(hex) => hex.clone(),
        }
    }

    /// Parse a wire string into a [`Color`]. A `1..=6` decimal is a preset;
    /// anything beginning with `#` is a hex literal. Returns `None` for input
    /// that is neither.
    #[must_use]
    pub fn parse_wire(raw: &str) -> Option<Self> {
        if let Some(slot) = parse_preset_slot(raw) {
            return Some(Self::Preset(slot));
        }
        if raw.starts_with('#') {
            return Some(Self::Hex(raw.to_owned()));
        }
        None
    }
}

/// Accept a bare `1..=6` decimal as a preset slot.
fn parse_preset_slot(raw: &str) -> Option<u8> {
    match raw.parse::<u8>() {
        Ok(slot @ 1..=6) => Some(slot),
        _ => None,
    }
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_wire_string())
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse_wire(&raw)
            .ok_or_else(|| DeError::invalid_value(Unexpected::Str(&raw), &"\"1\"..\"6\" or \"#RRGGBB\""))
    }
}

#[cfg(test)]
mod tests {
    use super::Color;

    #[test]
    fn preset_round_trips_as_bare_string() {
        let color = Color::Preset(4);
        let json = serde_json::to_string(&color).unwrap();
        assert_eq!(json, "\"4\"");
        assert_eq!(serde_json::from_str::<Color>(&json).unwrap(), color);
    }

    #[test]
    fn hex_round_trips_as_bare_string() {
        let color = Color::Hex("#ff8800".to_owned());
        let json = serde_json::to_string(&color).unwrap();
        assert_eq!(json, "\"#ff8800\"");
        assert_eq!(serde_json::from_str::<Color>(&json).unwrap(), color);
    }

    #[test]
    fn out_of_range_preset_is_treated_as_invalid() {
        assert!(Color::parse_wire("7").is_none());
        assert!(Color::parse_wire("0").is_none());
        assert!(serde_json::from_str::<Color>("\"9\"").is_err());
    }

    #[test]
    fn presets_one_through_six_parse() {
        for slot in 1..=6u8 {
            assert_eq!(Color::parse_wire(&slot.to_string()), Some(Color::Preset(slot)));
        }
    }
}
