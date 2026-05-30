//! # hiker-features
//!
//! Shareable feature implementations for `egui_workbench`-based apps.
//! Each feature declares its own *Ctx requirement trait* (e.g.
//! `FileTreeCtx`) and impls `egui_workbench::feature::Feature<C>` for
//! any `C: FooCtx + ?Sized`. Consumer apps (hiker-app, hiker-lite)
//! impl the requirement trait on their `CtxImpl` and pick the feature
//! up by including it in their registry's builtin list.
//!
//! See `docs/feature-registry.md` for the design and the per-feature
//! cost analysis. The pattern is opt-in per feature: features deeply
//! tied to one app stay in that app's tree and skip the trait dance.
//!
//! ## Adding a new shareable feature
//!
//! 1. Make a sub-module (e.g. `pub mod filetree;`).
//! 2. Define the Ctx requirement trait: `pub trait FileTreeCtx:
//!    egui_workbench::feature::Ctx { ... }`.
//! 3. Implement `Feature<C>` and the relevant surface sub-traits
//!    for any `C: FileTreeCtx + ?Sized`.
//! 4. Each consumer app impls `FileTreeCtx` for its `CtxImpl` (one
//!    method per accessor — usually a thin delegate to existing
//!    services) and supertraits it from the app umbrella Ctx trait.
//! 5. Add `Arc::new(FileTree) as Arc<dyn Feature<dyn AppCtx>>` to the
//!    app's builtins list.
//!
//! v1 ships no features yet — this crate is the home Phase 4+ will
//! populate as features migrate.

// Modules grow here as features migrate. Phase 4 candidates:
// - filetree   (currently in app/src/sidebar/files.rs)
// - find_replace (hiker-lite has a v1 in src/panels/find_replace.rs)
// - hex_view    (hiker-lite has a v1 in src/panels/hex_view.rs)
