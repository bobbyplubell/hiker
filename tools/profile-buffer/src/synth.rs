//! Synthetic corpus generation for the buffer decoration-rebuild profiler.
//!
//! Builds a markdown document with a configurable number of rendered diagrams
//! interspersed with prose paragraphs, plus the caret-offset sweep the timed
//! pass walks. Diagram sources are small but real (a 3-node mermaid graph / a
//! Pythagorean display-math block) so they actually parse, layout, and rasterize
//! once and populate the render caches the way a user's note would.

use anyhow::Result;

/// Which diagram family the corpus is built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagramKind {
    Mermaid,
    Math,
}

impl DiagramKind {
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "mermaid" => Ok(Self::Mermaid),
            "math" => Ok(Self::Math),
            other => anyhow::bail!("unknown --kind: {other} (want mermaid|math)"),
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Mermaid => "mermaid",
            Self::Math => "math",
        }
    }

    /// One self-contained diagram block, including a trailing blank line. Each
    /// is varied by `i` so distinct diagrams produce distinct render hashes (the
    /// realistic case — a note rarely repeats the exact same diagram), exercising
    /// one cache entry per block.
    fn block(self, i: usize) -> String {
        match self {
            Self::Mermaid => format!(
                "```mermaid\ngraph TD; A{i}-->B{i}; B{i}-->C{i};\n```\n\n"
            ),
            Self::Math => format!("$$\na_{{{i}}}^2 + b_{{{i}}}^2 = c_{{{i}}}^2\n$$\n\n"),
        }
    }
}

/// A prose paragraph, varied by `i`. Long enough to soft-wrap and to give the
/// caret sweep real in-prose offsets between the diagram blocks.
fn prose(i: usize) -> String {
    format!(
        "Paragraph {i}: lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do \
         eiusmod tempor incididunt ut labore et dolore magna aliqua. This is plain prose \
         line {i} with some **bold** and *italic* runs to exercise the markdown layer.\n\n"
    )
}

/// Build a document with `diagrams` blocks of `kind`, each preceded by a prose
/// paragraph (so the caret sweep crosses both prose and fence regions). When
/// `diagrams` is 0 the doc is prose only — the baseline with no widget layers to
/// rebuild.
pub fn doc(diagrams: usize, kind: DiagramKind) -> String {
    let mut s = String::new();
    s.push_str("# Profiler corpus\n\n");
    if diagrams == 0 {
        for i in 0..12 {
            s.push_str(&prose(i));
        }
        return s;
    }
    for i in 0..diagrams {
        s.push_str(&prose(i));
        s.push_str(&kind.block(i));
    }
    s
}

/// A sweep of `count` caret byte offsets across the document, snapped to char
/// boundaries. Evenly spaced, so the offsets land in BOTH prose and inside
/// diagram fences — the latter flips the per-block reveal state, the former is
/// the common idle-cursor case. Each consecutive offset differs, so every move
/// changes `sel_fp` and (per the bug) busts the whole-document widget layers.
pub fn caret_sweep(doc: &str, count: usize) -> Vec<usize> {
    let len = doc.len();
    if count == 0 || len == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(count);
    for k in 0..count {
        // Spread across (0, len); +1 in the divisor keeps the last sample off
        // the very end so it stays a valid interior caret position.
        let raw = (len * (k + 1)) / (count + 1);
        out.push(snap_to_char_boundary(doc, raw));
    }
    out
}

/// Round `byte` down to the nearest UTF-8 char boundary at or below it (the
/// synthetic corpus is ASCII, but be correct regardless).
fn snap_to_char_boundary(s: &str, byte: usize) -> usize {
    let mut b = byte.min(s.len());
    while b > 0 && !s.is_char_boundary(b) {
        b -= 1;
    }
    b
}
