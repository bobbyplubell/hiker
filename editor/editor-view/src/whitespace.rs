//! Visible-glyph decorations for tabs, spaces, NBSPs, zero-width characters,
//! and CRLF. See SPEC §9.16 and IMPLEMENTATION §16.6.7.

use editor_core::decoration::Decoration;

use editor_core::decoration::Set as DecorationSet;
use editor_core::state::Editor as EditorState;
use editor_core::rangeset::RangeSet;
use smol_str::SmolStr;

/// Toggles for which whitespace categories produce a visible-glyph Replace.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpecialCharsFlags {
    pub tabs: bool,
    pub spaces: bool,
    pub nbsp: bool,
    pub zero_width: bool,
    pub crlf: bool,
}

impl SpecialCharsFlags {
    pub const fn any(&self) -> bool {
        self.tabs || self.spaces || self.nbsp || self.zero_width || self.crlf
    }
}

const TAB_GLYPH: &str = "\u{2192}   ";
const SPACE_GLYPH: &str = "\u{00b7}";
const NBSP_GLYPH: &str = "\u{00b7}";
const ZW_GLYPH: &str = "\u{00b7}";
const CRLF_GLYPH: &str = "\u{21b5}";

/// Walk the doc; for each enabled category emit a `Replace` decoration over
/// the matching byte range with a visible glyph. Returns an empty set if
/// `flags.any()` is false.
pub fn special_chars_decorations(state: &EditorState, flags: SpecialCharsFlags) -> DecorationSet {
    if !flags.any() {
        return RangeSet::empty();
    }

    let doc = &state.doc;
    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();
    let mut byte: usize = 0;

    for chunk in doc.chunks() {
        let bytes = chunk.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            // Fast ASCII path.
            if b < 0x80 {
                match b {
                    b'\t' if flags.tabs => {
                        entries.push((
                            (byte + i)..(byte + i + 1),
                            Decoration::Replace {
                                display: Some(SmolStr::new_static(TAB_GLYPH)),
                            },
                        ));
                    }
                    b' ' if flags.spaces => {
                        entries.push((
                            (byte + i)..(byte + i + 1),
                            Decoration::Replace {
                                display: Some(SmolStr::new_static(SPACE_GLYPH)),
                            },
                        ));
                    }
                    b'\r' if flags.crlf => {
                        // Only mark \r when followed by \n.
                        let next = if i + 1 < bytes.len() {
                            Some(bytes[i + 1])
                        } else {
                            // Look across chunk boundary by peeking the doc.
                            let nb = byte + i + 1;
                            if nb < doc.len_bytes() {
                                Some(doc.byte_at(nb))
                            } else {
                                None
                            }
                        };
                        if next == Some(b'\n') {
                            entries.push((
                                (byte + i)..(byte + i + 2),
                                Decoration::Replace {
                                    display: Some(SmolStr::new_static(CRLF_GLYPH)),
                                },
                            ));
                            i += 2;
                            continue;
                        }
                    }
                    _ => {}
                }
                i += 1;
                continue;
            }

            // Multi-byte UTF-8: determine char length and decode.
            let cl = if b < 0xc0 {
                1
            } else if b < 0xe0 {
                2
            } else if b < 0xf0 {
                3
            } else {
                4
            };
            if i + cl > bytes.len() {
                // Char straddles chunk boundary; skip safely and let next chunk handle.
                break;
            }
            let s = match std::str::from_utf8(&bytes[i..i + cl]) {
                Ok(s) => s,
                Err(_) => {
                    i += 1;
                    continue;
                }
            };
            let ch = s.chars().next().unwrap();
            match ch {
                '\u{00a0}' if flags.nbsp => entries.push((
                    (byte + i)..(byte + i + cl),
                    Decoration::Replace {
                        display: Some(SmolStr::new_static(NBSP_GLYPH)),
                    },
                )),
                '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' if flags.zero_width => entries
                    .push((
                        (byte + i)..(byte + i + cl),
                        Decoration::Replace {
                            display: Some(SmolStr::new_static(ZW_GLYPH)),
                        },
                    )),
                _ => {}
            }
            i += cl;
        }
        byte += bytes.len();
    }

    RangeSet::from_iter(entries)
}

