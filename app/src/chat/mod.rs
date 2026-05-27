//! Chat panel scaffolding. The session model
//! and on-disk markdown store come from `hiker_core::sessions`; this
//! module wraps them with the egui-side in-memory state, channel
//! plumbing for async assistant replies, and the renderer shared
//! between the full-tab `panels::agent` view and the discovery panel's
//! docked region.
//!
//! Scope: session list + create + switch + delete, message list with
//! markdown + wikilink rendering, tool-call cards with per-card
//! collapse, @-mention autocomplete (indexer-backed), @selection
//! insertion, active-note context injection, animated typing indicator,
//! Stop button driven by a per-session StopSignal, input box with
//! Cmd-Enter send. `send::dispatch_reply` wires `core::agent::run_turn`
//! against the live MCP handler when one is attached and a no-op
//! dispatcher otherwise; assistant deltas stream into the transcript
//! through an mpsc channel pumped each frame.

pub mod md_preview;
pub mod render;
pub mod send;
pub mod session;
pub mod state;

