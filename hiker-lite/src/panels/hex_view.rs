//! Read-only hex dump renderer.

use crate::host::HexBuffer;

pub fn show(ui: &mut egui::Ui, buf: &HexBuffer) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.style_mut().override_font_id =
                Some(egui::FontId::monospace(13.0));
            for (offset, chunk) in buf.bytes.chunks(16).enumerate() {
                let mut line = String::with_capacity(80);
                line.push_str(&format!("{:08x}  ", offset * 16));
                for (i, b) in chunk.iter().enumerate() {
                    line.push_str(&format!("{b:02x} "));
                    if i == 7 {
                        line.push(' ');
                    }
                }
                // Pad the hex column so the ascii gutter aligns even on
                // short trailing rows.
                let padding_groups = 16 - chunk.len();
                for _ in 0..padding_groups {
                    line.push_str("   ");
                }
                line.push_str(" |");
                for b in chunk {
                    let c = *b;
                    if (0x20..0x7f).contains(&c) {
                        line.push(c as char);
                    } else {
                        line.push('.');
                    }
                }
                line.push('|');
                ui.label(line);
            }
        });
}
