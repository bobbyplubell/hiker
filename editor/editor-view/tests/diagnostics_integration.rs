//! Integration test: feeding diagnostics through `diagnostic_decorations`
//! produces both `Mark` (underline) and `Line` (gutter marker) decorations.

use editor_core::decoration::Decoration;

use editor_core::decoration::Diagnostic;

use editor_core::decoration::GutterMarker;

use editor_core::rope::Rope;

use editor_core::decoration::Severity;
use editor_view::diagnostics::diagnostic_decorations;
use smol_str::SmolStr;

#[test]
fn emits_mark_and_line_for_each_diagnostic() {
    // Three lines: "fn main() {\n    let x = 1;\n    bad();\n}"
    let text = "fn main() {\n    let x = 1;\n    bad();\n}\n";
    let doc = Rope::from_str(text);

    // Warning on `x` (line 1) and Error on `bad()` (line 2).
    let x_offset = text.find("let x").unwrap() + 4; // points at 'x'
    let warning_range = x_offset..x_offset + 1;

    let bad_offset = text.find("bad").unwrap();
    let error_range = bad_offset..bad_offset + 3;

    let diags = vec![
        Diagnostic {
            range: warning_range.clone(),
            severity: Severity::Warning,
            message: SmolStr::new_static("unused variable"),
            source: SmolStr::new_static("rustc"),
            code: Some(SmolStr::new_static("unused_variables")),
        },
        Diagnostic {
            range: error_range.clone(),
            severity: Severity::Error,
            message: SmolStr::new_static("cannot find function `bad`"),
            source: SmolStr::new_static("rustc"),
            code: Some(SmolStr::new_static("E0425")),
        },
    ];

    let decos = diagnostic_decorations(&diags, &doc, None);

    let mut has_warning_mark = false;
    let mut has_error_mark = false;
    let mut has_warning_line = false;
    let mut has_error_line = false;

    for (range, deco) in decos.iter_all() {
        match deco {
            Decoration::Mark(style) => {
                assert!(style.underline, "diagnostic marks must be underlined");
                if range == warning_range {
                    has_warning_mark = true;
                } else if range == error_range {
                    has_error_mark = true;
                }
            }
            Decoration::Line(style) => match &style.gutter_marker {
                Some(GutterMarker::Diagnostic(Severity::Warning)) => {
                    has_warning_line = true;
                }
                Some(GutterMarker::Diagnostic(Severity::Error)) => {
                    has_error_line = true;
                }
                _ => {}
            },
            _ => {}
        }
    }

    assert!(has_warning_mark, "expected an underline Mark over the warning range");
    assert!(has_error_mark, "expected an underline Mark over the error range");
    assert!(
        has_warning_line,
        "expected a Line decoration with a Warning gutter marker"
    );
    assert!(
        has_error_line,
        "expected a Line decoration with an Error gutter marker"
    );
}
