//! `editor-transactions-out` seam: a doc-mutating user-input edit applied by
//! the widget must surface on the `transactions_out` sink as the change set
//! it applied, while a selection-only / no-op input pushes nothing.

use editor_core::state::Editor as EditorState;
use editor_core::transaction::Transaction;
use editor_egui::widget::Widget as EditorWidget;
use editor_view::viewport::ViewState;

/// Click once at the text origin so the widget takes keyboard focus — the
/// key/text input path is focus-gated. Returns once focus has settled.
fn focus_widget(harness: &mut egui_kittest::Harness, x: f32, y: f32) {
    harness.input_mut().events.push(egui::Event::PointerButton {
        pos: egui::pos2(x, y),
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: Default::default(),
    });
    harness.input_mut().events.push(egui::Event::PointerButton {
        pos: egui::pos2(x, y),
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: Default::default(),
    });
    harness.run();
}

/// Press one key with the given modifiers, then run a frame.
fn press_mods(harness: &mut egui_kittest::Harness, key: egui::Key, modifiers: egui::Modifiers) {
    harness.input_mut().events.push(egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    });
    harness.run();
}

/// Press one key with the primary (ctrl) modifier set — the chord undo/redo
/// ride on. egui maps `ctrl` to the editor's `primary` off-mac.
fn press_primary(harness: &mut egui_kittest::Harness, key: egui::Key) {
    press_mods(harness, key, egui::Modifiers { ctrl: true, ..Default::default() });
}

/// Select `n` chars rightward from the current cursor with Shift+ArrowRight,
/// the way a user highlights text — keeping the selection alive through the
/// focus-gated key path (we can't poke `state.selection` while the harness
/// holds it borrowed).
fn select_right(harness: &mut egui_kittest::Harness, n: usize) {
    let shift = egui::Modifiers { shift: true, ..Default::default() };
    for _ in 0..n {
        press_mods(harness, egui::Key::ArrowRight, shift);
    }
}

#[test]
fn text_insert_is_captured_on_transactions_sink() {
    let mut state = EditorState::new("hello\n");
    let mut view = ViewState::default();
    let mut txs: Vec<Transaction> = Vec::new();

    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 600.0))
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view)
                    .with_transactions_sink(&mut txs)
                    .show(ui);
            });
        harness.run();
        // Focus first (no input handled yet), then type a character.
        focus_widget(&mut harness, 70.0, 10.0);
        harness.input_mut().events.push(egui::Event::Text("X".to_string()));
        harness.run();
    }

    assert_eq!(
        state.doc.to_string().len(),
        "hello\n".len() + 1,
        "the typed character should have grown the doc by one byte",
    );
    assert!(
        state.doc.to_string().contains('X'),
        "the typed character should have landed in the doc",
    );
    assert_eq!(
        txs.len(),
        1,
        "exactly one doc-mutating transaction should be captured, got {txs:?}",
    );
    assert!(
        !txs[0].changes.is_identity(),
        "captured transaction must carry a real (non-identity) change set",
    );
    // The change set inserts one byte: it grows the doc by the inserted length.
    assert_eq!(txs[0].changes.len_before(), "hello\n".len());
    assert_eq!(txs[0].changes.len_after(), "hello\n".len() + 1);
}

#[test]
fn selection_only_input_pushes_nothing() {
    let mut state = EditorState::new("hello\n");
    let mut view = ViewState::default();
    let mut txs: Vec<Transaction> = Vec::new();

    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 600.0))
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view)
                    .with_transactions_sink(&mut txs)
                    .show(ui);
            });
        harness.run();
        focus_widget(&mut harness, 70.0, 10.0);
        // An arrow-key motion changes only the selection — no doc change.
        harness.input_mut().events.push(egui::Event::Key {
            key: egui::Key::ArrowRight,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        });
        harness.run();
    }

    assert_eq!(
        state.doc.to_string(),
        "hello\n",
        "an arrow key must not change the doc",
    );
    assert!(
        txs.is_empty(),
        "selection-only input must push no transactions, got {txs:?}",
    );
}

#[test]
fn undo_after_typing_reverts_doc_and_is_captured_on_sink() {
    // Regression for "Ctrl+Z does nothing": undo must surface its inverse
    // change set on the sink, otherwise the host binding never mirrors it into
    // the working layer and the reverse pass restores the typed text.
    let mut state = EditorState::new("hello\n");
    let mut view = ViewState::default();
    let mut txs: Vec<Transaction> = Vec::new();
    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 600.0))
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view)
                    .with_transactions_sink(&mut txs)
                    .show(ui);
            });
        harness.run();
        focus_widget(&mut harness, 70.0, 10.0);
        // Home first so the insertion point is deterministic regardless of where
        // the focus click landed the cursor.
        press_mods(&mut harness, egui::Key::Home, egui::Modifiers::default());
        harness.input_mut().events.push(egui::Event::Text("X".to_string()));
        harness.run();
        press_primary(&mut harness, egui::Key::Z);
    }
    assert_eq!(state.doc.to_string(), "hello\n", "undo must revert the typed character");
    assert_eq!(txs.len(), 2, "the insert and its undo are both captured, got {txs:?}");
    // The second tx is the undo: applied to the typed doc, it reproduces the
    // pre-edit text (a real, non-identity change set).
    assert!(!txs[1].changes.is_identity(), "undo carries a real change set");
    assert_eq!(txs[1].changes.apply(&editor_core::rope::Rope::from_str("Xhello\n")).to_string(), "hello\n");
}

#[test]
fn redo_after_undo_reapplies_and_is_captured() {
    let mut state = EditorState::new("hello\n");
    let mut view = ViewState::default();
    let mut txs: Vec<Transaction> = Vec::new();
    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 600.0))
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view)
                    .with_transactions_sink(&mut txs)
                    .show(ui);
            });
        harness.run();
        focus_widget(&mut harness, 70.0, 10.0);
        press_mods(&mut harness, egui::Key::Home, egui::Modifiers::default());
        harness.input_mut().events.push(egui::Event::Text("X".to_string()));
        harness.run();
        press_primary(&mut harness, egui::Key::Z); // undo
        press_primary(&mut harness, egui::Key::Y); // redo
    }
    assert_eq!(state.doc.to_string(), "Xhello\n", "redo must reapply the typed character");
    assert_eq!(txs.len(), 3, "insert + undo + redo all captured, got {txs:?}");
    assert!(!txs[2].changes.is_identity(), "redo carries a real change set");
}

#[test]
fn cut_selection_writes_clipboard_and_captures_the_deletion() {
    // Ctrl+X over a selection: the deletion must surface on the sink (same bug
    // class as undo — without it the cut is reverted on the next reverse pass).
    // The selection is built with the keyboard (Home, then Shift+Right) so it
    // survives the focus-gated path.
    let mut state = EditorState::new("hello world\n");
    let mut view = ViewState::default();
    let mut txs: Vec<Transaction> = Vec::new();
    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 600.0))
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view)
                    .with_transactions_sink(&mut txs)
                    .show(ui);
            });
        harness.run();
        focus_widget(&mut harness, 70.0, 10.0);
        press_mods(&mut harness, egui::Key::Home, egui::Modifiers::default());
        select_right(&mut harness, 5); // highlight "hello"
        harness.input_mut().events.push(egui::Event::Cut);
        harness.run();
    }
    assert_eq!(state.doc.to_string(), " world\n", "cut deletes the highlighted selection");
    assert_eq!(txs.len(), 1, "the cut deletion is captured, got {txs:?}");
    assert_eq!(
        txs[0].changes.apply(&editor_core::rope::Rope::from_str("hello world\n")).to_string(),
        " world\n",
    );
}

#[test]
fn undo_after_replacing_a_selection_restores_it() {
    // Highlighted-text scenario: select a span, type over it (one replace
    // transaction), then undo — the original span must come back, and the undo
    // change set must reach the sink.
    let mut state = EditorState::new("hello world\n");
    let mut view = ViewState::default();
    let mut txs: Vec<Transaction> = Vec::new();
    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 600.0))
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view)
                    .with_transactions_sink(&mut txs)
                    .show(ui);
            });
        harness.run();
        focus_widget(&mut harness, 70.0, 10.0);
        press_mods(&mut harness, egui::Key::Home, egui::Modifiers::default());
        select_right(&mut harness, 5); // highlight "hello"
        harness.input_mut().events.push(egui::Event::Text("Hi".to_string()));
        harness.run();
        press_primary(&mut harness, egui::Key::Z);
    }
    assert_eq!(state.doc.to_string(), "hello world\n", "undo restores the replaced selection");
    // replace (delete+insert) captured, then undo captured.
    assert!(txs.len() >= 2, "replace and undo both captured, got {txs:?}");
    assert!(!txs.last().unwrap().changes.is_identity());
}
