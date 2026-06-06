//! SVG icon set. The UI refers to glyphs by name through the `ICONS`
//! singleton, e.g. `icons::ICONS.image(crate::icons::Icon::Trash)`. Each icon is backed
//! by a static byte slice loaded via `include_bytes!`; egui_extras' SVG
//! loader (installed in `HikerApp::new` via
//! `egui_extras::install_image_loaders`) rasterizes the bytes the first
//! time they're requested and caches the result by URI.
//!
//! The whole catalog routes through a single `image()` constructor keyed
//! by the `Icon` enum. One constructor (rather than one function per
//! glyph) keeps the "load + size" logic in exactly one place and means
//! adding a glyph is a one-line enum addition, not a new call-once method.

use eframe::egui;

/// Every glyph in the catalog. Each variant maps 1:1 to an SVG asset under
/// `app/assets/icons/`. The `icons!` macro wires the variant to its file
/// and bytes; see `Icons::image`.
macro_rules! icons {
    ($($variant:ident => $file:literal),+ $(,)?) => {
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub enum Icon {
            $($variant),+
        }

        impl Icon {
            /// `bytes://` URI egui caches the rasterized SVG under.
            const fn uri(self) -> &'static str {
                match self {
                    $(Self::$variant => concat!("bytes://icon-", $file)),+
                }
            }

            /// Static SVG source for this glyph.
            const fn bytes(self) -> &'static [u8] {
                match self {
                    $(Self::$variant => include_bytes!(
                        concat!("../assets/icons/", $file)
                    )),+
                }
            }
        }
    };
}

icons! {
    Trash => "trash.svg",
    Robot => "robot.svg",
    Brain => "brain.svg",
    Diff => "diff.svg",
    Restore => "restore.svg",
    Check => "check.svg",
    Cross => "cross.svg",
    Search => "search.svg",
    Settings => "settings.svg",
    Home => "home.svg",
    Graph => "graph.svg",
    Back => "back.svg",
    Forward => "forward.svg",
    Folder => "folder.svg",
    Expand => "expand.svg",
    Collapse => "collapse.svg",
    ChevronDown => "chevron_down.svg",
    ChevronRight => "chevron_right.svg",
    Close => "close.svg",
    Eye => "eye.svg",
    Wand => "wand.svg",
    Bookmark => "bookmark.svg",
    File => "file.svg",
    Dot => "dot.svg",
    Chat => "chat.svg",
    Clock => "clock.svg",
    Edit => "edit.svg",
    Clipboard => "clipboard.svg",
    Compass => "compass.svg",
    Wrench => "wrench.svg",
    Boot => "boot.svg",
    Walk => "walk.svg",
    Warning => "warning.svg",
    Chart => "chart.svg",
    Menu => "menu.svg",
    Plus => "plus.svg",
    Undo => "undo.svg",
    Redo => "redo.svg",
    WindowClose => "window_close.svg",
    WindowMaximize => "window_maximize.svg",
    WindowMinimize => "window_minimize.svg",
    Info => "info.svg",
    ClusterTree => "cluster_tree.svg",
    SidebarLeft => "sidebar_left.svg",
    SidebarRight => "sidebar_right.svg",
    Vault => "vault.svg",
    Cursor => "cursor.svg",
    Hand => "hand.svg",
    Canvas => "canvas.svg",
    Braces => "braces.svg",
    TabLink => "tab_link.svg",
    Book => "book.svg",
}

pub struct Icons;
pub const ICONS: Icons = Icons;

impl Icons {
    /// Build the 14x14 egui image for `icon`. Every UI icon goes through
    /// here so sizing and the cache URI stay in one spot.
    pub fn image(&self, icon: Icon) -> egui::Image<'static> {
        egui::Image::new(egui::ImageSource::Bytes {
            uri: icon.uri().into(),
            bytes: egui::load::Bytes::Static(icon.bytes()),
        })
        .fit_to_exact_size(egui::vec2(14.0, 14.0))
    }

    /// `boot.svg` is the squiggly-trail glyph used on trail surfaces
    /// (sidebar header, editor pill, etc.).
    pub fn trail(&self) -> egui::Image<'static> {
        self.image(crate::icons::Icon::Boot)
    }

    // ---- Semantic helpers ------------------------------------------------
    //
    // Every place the UI puts a tinted icon should call one of these so the
    // "what" (which symbol) and the "how it's coloured" (which role) stay in
    // one place. Adding a new role here is cheap; sprinkling raw `.tint(...)`
    // calls across panels is what we're trying to avoid.

    /// Amber-tinted warning glyph: stale buffer, indexer offline, tool error
    /// in chat. Pair with `hiker_theme::warn()` for matching text colour.
    pub fn warn(&self) -> egui::Image<'static> {
        self.image(crate::icons::Icon::Warning).tint(hiker_theme::warn())
    }

    /// White-tinted check used inside coloured "primary affirmative"
    /// buttons (Apply staging, accept patch, etc.).
    pub fn primary_check(&self) -> egui::Image<'static> {
        self.image(crate::icons::Icon::Check).tint(egui::Color32::WHITE)
    }

    /// White-tinted cross used inside coloured "primary destructive"
    /// buttons (Reject staging, discard, etc.).
    pub fn primary_cross(&self) -> egui::Image<'static> {
        self.image(crate::icons::Icon::Cross).tint(egui::Color32::WHITE)
    }

    /// White-tinted restore used inside coloured "primary restore"
    /// buttons (Restore from trash, restore snapshot).
    pub fn primary_restore(&self) -> egui::Image<'static> {
        self.image(crate::icons::Icon::Restore).tint(egui::Color32::WHITE)
    }

    /// Accent-tinted dot used as a "current selection" marker in the buffer
    /// header / status bar (dirty / active indicators).
    pub fn current_dot(&self) -> egui::Image<'static> {
        self.image(crate::icons::Icon::Dot).tint(hiker_theme::accent())
    }
}
