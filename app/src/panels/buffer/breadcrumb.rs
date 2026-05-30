//! Heading breadcrumb for a buffer: walk from the start of the doc up
//! through the cursor's line and produce a `/`-joined ATX-heading stack.
//! Pulled out of `mod.rs` to keep that file under the length budget.

use crate::buffer::Buffer;

/// Heading-breadcrumb lookup on a buffer. Standalone trait so the tests
/// can call it without going through the panel context.
pub(super) trait HeadingBreadcrumb {
    /// Walk the document from the start up through the cursor's line
    /// and return a `>`-joined breadcrumb of the active heading stack.
    fn heading_breadcrumb(&self) -> String;
}

impl HeadingBreadcrumb for Buffer {
    fn heading_breadcrumb(&self) -> String {
        let cursor_line = self
            .editor
            .doc
            .byte_to_line(self.editor.selection.main().head.byte as usize);
        let mut stack: Vec<(u8, String)> = Vec::new();
        let total_lines = self.editor.doc.len_lines();
        for line_idx in 0..=cursor_line {
            let start = self.editor.doc.line_to_byte(line_idx);
            let end = if line_idx + 1 < total_lines {
                self.editor.doc.line_to_byte(line_idx + 1)
            } else {
                self.editor.doc.len_bytes()
            };
            let line: String = self.editor.doc.slice(start..end).to_string();
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix('#') {
                let mut depth: u8 = 1;
                let mut chars = rest.chars();
                for c in chars.by_ref() {
                    if c == '#' && depth < 6 {
                        depth += 1;
                    } else if c == ' ' || c == '\t' {
                        break;
                    } else {
                        depth = 0;
                        break;
                    }
                }
                if depth == 0 {
                    continue;
                }
                let title = chars.as_str().trim_end_matches(['\n', '\r']).trim();
                stack.retain(|(d, _)| *d < depth);
                stack.push((depth, title.to_string()));
            }
        }
        stack.into_iter().map(|(_, t)| t).collect::<Vec<_>>().join(" /")
    }
}

#[cfg(test)]
mod breadcrumb_tests {
    use super::*;

    fn make(text: &str, cursor_byte: usize) -> Buffer {
        let mut buf = Buffer::with_config_and_vault(
            "test.md".to_string(),
            text,
            String::new(),
            None,
            None,
        );
        buf.editor.selection = editor_core::selection::Selection::single(cursor_byte);
        buf
    }

    #[test]
    fn empty_when_no_headings() {
        let buf = make("just some text\nno heads here\n", 0);
        assert_eq!(buf.heading_breadcrumb(), "");
    }

    #[test]
    fn picks_up_h1() {
        let buf = make("# Title\nbody\n", 9); // cursor on `body`
        assert_eq!(buf.heading_breadcrumb(), "Title");
    }

    #[test]
    fn stacks_deeper_headings() {
        let text = "# A\n## B\n### C\nbody\n";
        let byte = text.find("body").unwrap();
        let buf = make(text, byte);
        assert_eq!(buf.heading_breadcrumb(), "A /B /C");
    }

    #[test]
    fn higher_heading_resets_deeper_stack() {
        let text = "# A\n## B\n### C\n## D\nbody\n";
        let byte = text.find("body").unwrap();
        let buf = make(text, byte);
        assert_eq!(buf.heading_breadcrumb(), "A /D");
    }
}
