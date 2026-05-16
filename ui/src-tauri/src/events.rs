//! Typed event registry. Single source of truth for the `hiker:*` events
//! emitted by the Rust backend to the Tauri frontend. Each variant's
//! payload type matches what the JS `listen()` call receives.
//!
//! Why this exists: every `app.emit("hiker:foo", &payload)` call site
//! used to hand-type both the event-name string and the payload type,
//! and the JS side hand-typed them again on the `listen<T>(...)` end.
//! A renamed Rust field silently broke TS. The helpers below force the
//! payload type at the call site; the wire-name strings live only in
//! the `name` module, mirrored on the TS side by `ui/src/events.ts`.
//!
//! KEEP IN SYNC with `ui/src/events.ts` — the TS `HikerEvents`
//! interface mirrors `name::*` and payload shapes one-for-one.

use serde::Serialize;
use tauri::Emitter;

use hiker_core::agent::AgentEvent;
use hiker_core::changes::ChangeRow;
use hiker_core::config::Config;
use hiker_core::indexer::{IndexStatus, ProgressEvent};
use hiker_core::tasks::QueueEvent;
use hiker_core::watcher::FileEvent;

/// Compile-time enumeration of every event name. Each constant is the
/// canonical wire string; the typed `emit_*` helpers below verify the
/// payload type at the call site.
pub mod name {
    pub const CHANGES_APPENDED: &str = "hiker:changes-appended";
    pub const CHAT_EVENT: &str = "hiker:chat-event";
    pub const CONFIG_RELOADED: &str = "hiker:config-reloaded";
    pub const FILE_CHANGED: &str = "hiker:file-changed";
    pub const INDEX_STATUS: &str = "hiker:index-status";
    pub const LLM_WARNING: &str = "hiker:llm-warning";
    pub const NOTE_MUTATION_APPLIED: &str = "hiker:note-mutation-applied";
    pub const NOTE_MUTATION_FAILED: &str = "hiker:note-mutation-failed";
    pub const QUEUE_EVENT: &str = "hiker:queue-event";
    pub const REINDEX_PROGRESS: &str = "hiker:reindex-progress";
    pub const STAGING_CHANGED: &str = "hiker:staging-changed";
    pub const TRASH_CHANGED: &str = "hiker:trash-changed";
    pub const WATCHER_OVERFLOW: &str = "hiker:watcher-overflow";
}

/// `hiker:llm-warning` payload. Hand-rolled struct (vs the previous
/// inline `serde_json::json!`) so the helper can require the right
/// shape at the call site.
#[derive(Debug, Clone, Serialize)]
pub struct LlmWarning<'a> {
    pub kind: &'a str,
    pub env: &'a str,
    pub message: String,
}

pub fn emit_changes_appended(app: &tauri::AppHandle, payload: &ChangeRow) {
    let _ = app.emit(name::CHANGES_APPENDED, payload);
}

pub fn emit_chat_event(app: &tauri::AppHandle, payload: &AgentEvent) {
    let _ = app.emit(name::CHAT_EVENT, payload);
}

pub fn emit_config_reloaded(app: &tauri::AppHandle, payload: &Config) {
    let _ = app.emit(name::CONFIG_RELOADED, payload);
}

pub fn emit_file_changed(app: &tauri::AppHandle, payload: &FileEvent) {
    let _ = app.emit(name::FILE_CHANGED, payload);
}

pub fn emit_index_status(app: &tauri::AppHandle, payload: &IndexStatus) {
    let _ = app.emit(name::INDEX_STATUS, payload);
}

pub fn emit_llm_warning(app: &tauri::AppHandle, payload: &LlmWarning<'_>) {
    let _ = app.emit(name::LLM_WARNING, payload);
}

pub fn emit_note_mutation_applied<P: Serialize>(app: &tauri::AppHandle, payload: &P) {
    let _ = app.emit(name::NOTE_MUTATION_APPLIED, payload);
}

pub fn emit_note_mutation_failed<P: Serialize>(app: &tauri::AppHandle, payload: &P) {
    let _ = app.emit(name::NOTE_MUTATION_FAILED, payload);
}

pub fn emit_queue_event(app: &tauri::AppHandle, payload: &QueueEvent) {
    let _ = app.emit(name::QUEUE_EVENT, payload);
}

pub fn emit_reindex_progress(app: &tauri::AppHandle, payload: &ProgressEvent) {
    let _ = app.emit(name::REINDEX_PROGRESS, payload);
}

pub fn emit_staging_changed(app: &tauri::AppHandle) {
    let _ = app.emit(name::STAGING_CHANGED, ());
}

pub fn emit_trash_changed(app: &tauri::AppHandle) {
    let _ = app.emit(name::TRASH_CHANGED, ());
}

pub fn emit_watcher_overflow(app: &tauri::AppHandle) {
    let _ = app.emit(name::WATCHER_OVERFLOW, ());
}
