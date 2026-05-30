//! Markdown language support: parses doc text and produces decoration sets
//! for live-preview rendering plus a fold model for headings + lists.

pub mod admonitions;
pub mod completion;
pub mod styling;
pub mod folds;
pub mod notes;
pub mod meta;
pub mod indenter;
pub mod equations;
pub mod diagrams;
pub mod embeds;
pub mod links;
pub mod syntax;

