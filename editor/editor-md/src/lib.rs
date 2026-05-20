//! Markdown language support: parses doc text and produces decoration sets
//! for live-preview rendering plus a fold model for headings + lists.

pub mod callout;
pub mod completion;
pub mod decorations;
pub mod folds;
pub mod footnote;
pub mod frontmatter;
pub mod indent;
pub mod math;
pub mod mermaid;
pub mod transclusion;
pub mod wikilink;

pub use callout::{callout_decorations, CalloutType};
pub use completion::WikilinkSource;
pub use decorations::markdown_decorations;
pub use folds::{fold_decorations, fold_regions, FoldKind, FoldRegion, FoldState};
pub use footnote::{footnote_decorations, COLOR_FOOTNOTE, COLOR_FOOTNOTE_DEF_BG};
pub use frontmatter::{frontmatter_fold, FRONTMATTER_FOLD_ID};
pub use indent::{markdown_indent_on_enter, MarkdownIndent};
pub use math::{math_decorations, COLOR_MATH_BG, COLOR_MATH_FG};
pub use mermaid::{mermaid_decorations, COLOR_MERMAID_BG};
pub use transclusion::{transclusion_decorations, COLOR_TRANSCLUSION};
pub use wikilink::{wikilink_decorations, COLOR_WIKILINK};
