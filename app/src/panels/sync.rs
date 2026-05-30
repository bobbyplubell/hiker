//! Sync detail tab: device identity, enrollment, force-sync / discovery,
//! recent synced items, and the live progress log.
//!
//! Mirrors `indexer_detail.rs`: a header grid built from live service state,
//! action buttons, a filter-pilled scrollable event log. State pulled from
//! `AppState::vault_session.services.sync` (the live `SyncService`, present
//! only when `[sync].enabled`) plus the `sync_events` ring that
//! `main::drain_sync_events` feeds from the service's progress channel, plus a
//! query over the op log for `author LIKE 'sync:%'` (recently synced items).
//!
//! ## Content key
//!
//! The vault content key transfers in-band automatically after enrollment
//! (`sync-vault-key-inband`): on first contact the non-canonical device adopts
//! the canonical device's key over the authenticated channel. The page's
//! Content-key Copy/Import is a manual fallback.

use std::sync::Arc;
use std::time::Duration;

use eframe::egui;

use hiker_core::activity::{Filter, Source, Summary};
use hiker_sync::config::SyncMode;
use hiker_sync::identity::Resolution;

use crate::state::{AppState, ToastLevel};
use crate::theme;

/// The mDNS discovery window length (matches `docs/sync.md`'s ~30s manual,
/// time-boxed window). [sync-mdns-discovery]
const DISCOVERY_WINDOW: Duration = Duration::from_secs(30);

/// How many diff lines the inline fork-diff renders before truncating with a
/// "…" marker — it lives inside the page scroll, so a huge fork stays bounded.
/// [sync-fork-diff]
const FORK_DIFF_MAX_LINES: usize = 200;

/// Per-fork "view diff" state, keyed by the forked doc's path in the Sync
/// page's [`State`] cache. Set to `Fetching` on the View-diff click, then
/// resolved by `main::drain_fork_diff_results` to `Ready` (the peer's current
/// text) or `Error`. [sync-fork-diff]
#[derive(Clone)]
pub enum ForkDiffState {
    /// The peer's version is being fetched on demand.
    Fetching,
    /// The peer's current text, ready to diff against ours.
    Ready(String),
    /// The fetch failed (peer offline, transport error, …).
    Error(String),
}

/// Sync page local UI state: the per-fork "view diff" cache. Read-only preview
/// content fetched on demand; never mutates our doc. [sync-fork-diff]
#[derive(Default)]
pub struct State {
    /// Forked-doc path → its current "view diff" state. Populated by the
    /// View-diff button (sets `Fetching`) and the per-frame drain of the
    /// fork-diff result channel (`Ready` / `Error`).
    pub fork_diffs: std::collections::HashMap<String, ForkDiffState>,
}

pub fn show(
    ui: &mut egui::Ui,
    app: &mut AppState,
    rt: &Arc<tokio::runtime::Runtime>,
) {
    // When sync is disabled there's no service: render a hint and bail. (Fetch
    // the service before the heading so the header can carry a conflict count.)
    let service_for_header = app.vault_session.services.sync.clone();
    // From the cheap `Shared` snapshot — never lock the node on the render path
    // (the responder/auto-sync task holds the node lock for whole windows, so a
    // blocking_lock here would stall the UI).
    let blocked = service_for_header
        .as_ref()
        .map(|s| s.state_snapshot().blocked)
        .unwrap_or_default();
    ui.horizontal(|ui| {
        ui.heading("Sync");
        if !blocked.is_empty() {
            ui.label(
                egui::RichText::new(format!("\u{26A0} {} conflicts", blocked.len()))
                    .color(egui::Color32::from_rgb(210, 150, 40))
                    .strong(),
            );
        }
    });
    ui.add_space(4.0);

    // When sync is disabled there's no service: offer to enable it and bail.
    let Some(service) = app.vault_session.services.sync.clone() else {
        ui.label(egui::RichText::new("Sync is disabled for this vault.").color(theme::muted()));
        ui.add_space(6.0);
        if ui.button("Enable sync").clicked() {
            set_sync_enabled(app, true);
        }
        return;
    };

    let snap = service.state_snapshot();

    // Wrap the whole page body in a single vertical scroll area so everything
    // below the fixed "Sync" header can be reached. The body is extracted into
    // `sync_body` (rather than inlined as a closure) to keep the `&mut app`
    // borrow plus the other borrows clean for the checker.
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            sync_body(ui, app, &service, &snap, &blocked, rt);
        });
}

/// Flip `[sync].enabled` in the vault config and swap the in-memory copy, so
/// the per-frame `reconcile_sync` builds (enable) or tears down (disable) the
/// engine immediately — no vault reopen. Mirrors the Settings pane's `commit`
/// path; `sync.enabled` is write-eligible at vault scope. [sync-disable-kill-switch]
fn set_sync_enabled(app: &mut AppState, enabled: bool) {
    let vault_root = app.vault_session.vault_root.clone();
    match hiker_core::config::Config::set(
        hiker_core::config::SettingsScope::Vault,
        "sync.enabled",
        &serde_json::Value::Bool(enabled),
        &vault_root,
    ) {
        Ok(new_cfg) => {
            if let Ok(mut guard) = app.vault_session.config.write() {
                *guard = new_cfg;
            }
            app.push_toast(
                if enabled { "Sync enabled" } else { "Sync disabled" },
                ToastLevel::Info,
            );
        }
        Err(e) => {
            let verb = if enabled { "enable" } else { "disable" };
            app.push_toast(format!("Failed to {verb} sync: {e}"), ToastLevel::Error);
        }
    }
}

/// The read-only detail grid on the Sync page: enabled / mode / server URL /
/// this device's fingerprint (with a Copy button) / last-sync summary.
fn sync_detail_grid(ui: &mut egui::Ui, snap: &crate::sync_service::SyncSnapshot) {
    egui::Grid::new("sync-detail-grid")
        .num_columns(2)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            ui.label(egui::RichText::new("Enabled").color(theme::muted()));
            ui.label(if snap.enabled { "yes" } else { "no" });
            ui.end_row();

            ui.label(egui::RichText::new("Mode").color(theme::muted()));
            ui.label(match snap.mode {
                SyncMode::Peer => "peer (LAN)",
                SyncMode::Server => "server",
                SyncMode::Both => "both",
            });
            ui.end_row();

            ui.label(egui::RichText::new("Server URL").color(theme::muted()));
            ui.label(if snap.server_url.is_empty() {
                "(none — LAN only)".to_string()
            } else {
                snap.server_url.clone()
            });
            ui.end_row();

            ui.label(egui::RichText::new("This device").color(theme::muted()));
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&snap.fingerprint).monospace().small());
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text(snap.fingerprint.clone());
                }
            });
            ui.end_row();

            ui.label(egui::RichText::new("Last sync").color(theme::muted()));
            ui.label(match snap.last_sync_ms {
                Some(ms) => format_last_sync(ms, snap.last_report.as_ref()),
                None => "(never)".to_string(),
            });
            ui.end_row();
        });
}

/// The editable "Device name" row: this device names ITSELF, and that name is
/// carried on the sync handshake so peers show it instead of a fingerprint. The
/// draft lives in egui memory; Set persists it to `[sync].device_name`.
/// [sync-device-name]
fn device_name_section(
    ui: &mut egui::Ui,
    app: &mut AppState,
    service: &Arc<crate::sync_service::SyncService>,
    snap: &crate::sync_service::SyncSnapshot,
) {
    ui.label(egui::RichText::new("This device's name").color(theme::muted()));
    let name_id = egui::Id::new("sync-device-name-draft");
    // Seed the draft from the current configured name on first render.
    let mut draft: String = ui
        .ctx()
        .data(|d| d.get_temp::<String>(name_id))
        .unwrap_or_else(|| snap.device_name.clone());
    let mut do_set = false;
    ui.horizontal(|ui| {
        let resp = ui.add(
            egui::TextEdit::singleline(&mut draft)
                .hint_text("e.g. laptop, work desktop")
                .desired_width(240.0),
        );
        let submit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ui.button("Set name").clicked() || submit {
            do_set = true;
        }
    });
    if do_set {
        match service.set_device_name(&app.vault_session.vault_root, draft.trim()) {
            Ok(()) => app.push_toast("Device name set", ToastLevel::Info),
            Err(e) => app.push_toast(format!("Set name failed: {e}"), ToastLevel::Error),
        }
    }
    ui.ctx().data_mut(|d| d.insert_temp(name_id, draft));
}

/// The scrollable body of the Sync page: everything below the fixed header
/// (config warning, last error, conflicts, the detail grid, content key,
/// enrolled devices, enroll/connect fields, action buttons, discovered peers,
/// recent synced items, and the progress log). Rendered inside the single page
/// scroll area, so it contains no nested same-direction `ScrollArea`s.
fn sync_body(
    ui: &mut egui::Ui,
    app: &mut AppState,
    service: &Arc<crate::sync_service::SyncService>,
    snap: &crate::sync_service::SyncSnapshot,
    blocked: &[hiker_sync::identity::BlockedDoc],
    rt: &Arc<tokio::runtime::Runtime>,
) {
    // Config sanity: surface a prominent amber warning when the running config
    // can never converge, so a misconfigured vault doesn't fail silently.
    if let Some(warn) = config_warning(snap) {
        ui.label(
            egui::RichText::new(warn)
                .color(egui::Color32::from_rgb(210, 150, 40))
                .strong(),
        );
        ui.add_space(4.0);
    }

    // Surfaced last error (notably a content-key mismatch). Red, actionable.
    if let Some(err) = &snap.last_error {
        ui.label(
            egui::RichText::new(format!("Last error: {err}"))
                .color(egui::Color32::from_rgb(200, 60, 60))
                .strong(),
        );
        ui.add_space(4.0);
    }

    // Conflicts (forked docs): only shown when there are any. Each row offers
    // the keep-mine / keep-theirs / keep-both verbs. [sync-blocked-state]
    if !blocked.is_empty() {
        conflicts_section(ui, app, service, blocked, rt);
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(6.0);
    }

    if ui.button("Disable sync").clicked() {
        set_sync_enabled(app, false);
    }
    ui.add_space(6.0);

    sync_detail_grid(ui, snap);

    ui.add_space(6.0);

    // This device's self-set name, carried on the handshake so peers show it
    // instead of a fingerprint. Editable here; persists to [sync].device_name.
    // [sync-device-name]
    device_name_section(ui, app, service, snap);

    ui.add_space(10.0);
    ui.separator();

    // --- Content key ----------------------------------------------------
    // The manual stand-in for the in-band key handshake (`sync-vault-key-inband`):
    // two of a user's own devices must share the same content key to decrypt
    // each other. Copy it here, paste + Import on the other device.
    content_key_section(ui, app, service, rt);

    ui.add_space(10.0);
    ui.separator();

    // --- Enrolled devices (rename + remove rows) ------------------------
    ui.label(egui::RichText::new("Enrolled devices").color(theme::muted()));
    enrolled_devices_section(ui, app, service, rt);

    ui.add_space(8.0);

    // Enroll a peer by its swapped fingerprint. The draft lives in egui memory
    // so it survives across renders without leaking into AppState.
    ui.label(egui::RichText::new("Enroll device").color(theme::muted()));
    let enroll_id = egui::Id::new("sync-enroll-draft");
    let mut draft: String = ui
        .ctx()
        .data(|d| d.get_temp::<String>(enroll_id))
        .unwrap_or_default();
    let mut do_enroll = false;
    ui.horizontal(|ui| {
        let resp = ui.add(
            egui::TextEdit::singleline(&mut draft)
                .hint_text("paste a peer device fingerprint")
                .desired_width(360.0),
        );
        let submit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ui.button("Enroll").clicked() || submit {
            do_enroll = true;
        }
    });
    if do_enroll {
        let fp = draft.trim().to_string();
        match service.enroll_device(&app.vault_session.vault_root, &fp, rt) {
            Ok(()) => {
                app.push_toast("Device enrolled", ToastLevel::Info);
                draft.clear();
            }
            Err(e) => {
                app.push_toast(format!("Enroll failed: {e}"), ToastLevel::Error);
            }
        }
    }
    ui.ctx().data_mut(|d| d.insert_temp(enroll_id, draft));

    ui.add_space(8.0);

    // Manual on-demand actions (auto-sync already runs every ~15s).
    ui.horizontal(|ui| {
        if ui.button("Sync now").clicked() {
            service.force_sync(rt);
            app.push_toast("Sync requested", ToastLevel::Info);
        }
        if ui.button("Discover (30s)").clicked() {
            service.discover(DISCOVERY_WINDOW, rt);
            app.push_toast("Discovery started", ToastLevel::Info);
        }
    });

    ui.add_space(8.0);

    // Discovered-peer visibility: what mDNS is currently surfacing, split into
    // enrolled (reachable, will sync) and seen-but-unenrolled (needs enrolling).
    // A seen peer whose fingerprint can be derived gets a one-click Enroll.
    discovered_peers_section(ui, app, service, snap, rt);

    ui.add_space(8.0);

    // Manual peer fallback: dial an explicit multiaddr when mDNS finds nothing.
    // Still gated on the peer being enrolled (the transport auth gate enforces).
    ui.label(egui::RichText::new("Connect to peer address").color(theme::muted()));
    let peer_id = egui::Id::new("sync-peer-addr-draft");
    let mut peer_draft: String = ui
        .ctx()
        .data(|d| d.get_temp::<String>(peer_id))
        .unwrap_or_default();
    let mut do_connect = false;
    ui.horizontal(|ui| {
        let resp = ui.add(
            egui::TextEdit::singleline(&mut peer_draft)
                .hint_text("/ip4/192.168.x.y/tcp/PORT")
                .desired_width(360.0),
        );
        let submit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ui.button("Connect").clicked() || submit {
            do_connect = true;
        }
    });
    if do_connect {
        let addr = peer_draft.trim().to_string();
        if addr.is_empty() {
            app.push_toast("Enter a peer address first", ToastLevel::Error);
        } else {
            service.connect_to(&addr, rt);
            app.push_toast("Connecting to peer", ToastLevel::Info);
        }
    }
    ui.ctx().data_mut(|d| d.insert_temp(peer_id, peer_draft));

    ui.add_space(12.0);
    ui.separator();

    // Recent synced items: query the op log for `author LIKE 'sync:%'`.
    ui.label(egui::RichText::new("Recent synced items").color(theme::muted()));
    let activity = app.vault_session.services.activity.clone();
    let synced = activity.list(&Filter {
        source: Source::ChangesOnly,
        limit: 50,
        author_pattern: Some("sync:%".to_string()),
        since_ms: None,
    });
    match synced {
        Ok(items) if !items.is_empty() => {
            // Rendered inline within the single page scroll (no nested
            // same-direction ScrollArea). Bounded by the query's `take(50)`.
            for item in items.iter().take(50) {
                ui.label(egui::RichText::new(synced_item_line(item)).small());
            }
        }
        Ok(_) => {
            ui.label(
                egui::RichText::new("(no synced items yet)")
                    .color(theme::muted())
                    .italics()
                    .small(),
            );
        }
        Err(err) => {
            ui.colored_label(
                egui::Color32::from_rgb(200, 60, 60),
                format!("Failed to load synced items: {err}"),
            );
        }
    }

    ui.add_space(8.0);
    ui.separator();
    ui.label(egui::RichText::new("Progress").color(theme::muted()));

    // Filter pills over the progress log, mirroring indexer_detail.
    let mem_id = egui::Id::new("sync-events-filter");
    let mut filter: String = ui
        .ctx()
        .data(|d| d.get_temp::<String>(mem_id))
        .unwrap_or_default();
    ui.horizontal(|ui| {
        let all_sel = filter.is_empty();
        if ui.selectable_label(all_sel, "All").clicked() {
            filter.clear();
        }
        // The progress lines say "sent"/"received" only once those flows are
        // streaming; the pills below match the substrings we currently emit
        // plus the forward-looking sent/received labels.
        for pill in ["sent", "received", "conflict", "error", "peer"] {
            let sel = filter == pill;
            if ui.selectable_label(sel, pill).clicked() {
                filter = if sel { String::new() } else { pill.to_string() };
            }
        }
    });
    ui.ctx().data_mut(|d| d.insert_temp(mem_id, filter.clone()));

    // Rendered inline within the single page scroll (no nested same-direction
    // ScrollArea — the old `max_height(available_height())` is unbounded inside
    // an outer scroll). Bounded: scans the last 200 events, shows up to 50.
    let n = app.vault_session.events.sync_events.len();
    let start = n.saturating_sub(200);
    let mut shown = 0usize;
    for line in app.vault_session.events.sync_events.iter().skip(start) {
        if !filter.is_empty() && !line.to_ascii_lowercase().contains(&filter) {
            continue;
        }
        ui.label(egui::RichText::new(line).monospace().small());
        shown += 1;
        if shown >= 50 {
            break;
        }
    }
    if n == 0 {
        ui.label(
            egui::RichText::new("(no events yet)")
                .color(theme::muted())
                .italics()
                .small(),
        );
    } else if shown == 0 {
        ui.label(
            egui::RichText::new(format!("(no events match '{}')", filter))
                .color(theme::muted())
                .italics()
                .small(),
        );
    }
}

/// A prominent config-sanity warning for a config that can never converge, or
/// `None` when the config has a plausible path to a peer. [sync-config-section]
fn config_warning(snap: &crate::sync_service::SyncSnapshot) -> Option<String> {
    if !snap.enabled {
        return None;
    }
    match snap.mode {
        SyncMode::Server | SyncMode::Both if snap.server_url.is_empty() => Some(
            "Server mode but no server URL set — configure [sync].server_url."
                .to_string(),
        ),
        // Peer-only (no server) with discovery off can't find anyone on the LAN.
        SyncMode::Peer if !snap.discovery => Some(
            "Peer mode with discovery off — no way to find peers; enable \
             discovery or add a server."
                .to_string(),
        ),
        _ => None,
    }
}

/// The "Discovered on LAN" section: what mDNS is currently surfacing, read from
/// the cheap `Shared` snapshot (NEVER locks the node on render). Two lists:
/// enrolled candidates (reachable, will sync) and hiker peers seen but not
/// enrolled (can't sync until enrolled — but surfaced so the user knows
/// something is on the network). [sync-mdns-discovery]
fn discovered_peers_section(
    ui: &mut egui::Ui,
    app: &mut AppState,
    service: &Arc<crate::sync_service::SyncService>,
    snap: &crate::sync_service::SyncSnapshot,
    rt: &Arc<tokio::runtime::Runtime>,
) {
    ui.label(egui::RichText::new("Discovered on LAN (enrolled)").color(theme::muted()));
    if snap.discovered.is_empty() {
        ui.label(
            egui::RichText::new("(no enrolled peers visible right now)")
                .color(theme::muted())
                .italics()
                .small(),
        );
    } else {
        for (fp, addr) in &snap.discovered {
            // Local alias override, else the learned synced name, else a
            // truncated fingerprint; full fp on hover. [sync-device-name]
            let name = service
                .display_name(fp)
                .unwrap_or_else(|| truncate_mono(fp, 18));
            ui.label(egui::RichText::new(format!("{name} \u{2014} {addr}")).monospace().small())
                .on_hover_text(fp);
        }
    }

    ui.add_space(4.0);
    ui.label(egui::RichText::new("Seen on LAN (not enrolled)").color(theme::muted()));
    if snap.seen_unenrolled.is_empty() {
        ui.label(
            egui::RichText::new("(none)")
                .color(theme::muted())
                .italics()
                .small(),
        );
    } else {
        ui.label(
            egui::RichText::new(
                "A hiker instance is on the network but isn't enrolled. Verify this \
                 fingerprint matches the one shown on that device before enrolling.",
            )
            .color(egui::Color32::from_rgb(180, 140, 60))
            .small()
            .italics(),
        );
        for (peer_id, addr, fingerprint) in &snap.seen_unenrolled {
            ui.horizontal(|ui| {
                match fingerprint {
                    // Fingerprint derivable from the PeerId: show it (the user
                    // verifies it against the other device) and offer a one-click
                    // enroll, the convenience over copy-pasting it.
                    Some(fp) => {
                        ui.label(
                            egui::RichText::new(format!("{} \u{2014} {addr}", truncate_mono(fp, 18)))
                                .monospace()
                                .small(),
                        )
                        .on_hover_text(fp);
                        if ui.small_button("Enroll").clicked() {
                            match service.enroll_device(
                                &app.vault_session.vault_root,
                                fp,
                                rt,
                            ) {
                                Ok(()) => app.push_toast("Device enrolled", ToastLevel::Info),
                                Err(e) => app
                                    .push_toast(format!("Enroll failed: {e}"), ToastLevel::Error),
                            }
                        }
                    }
                    // Can't derive a fingerprint from this PeerId: show it
                    // (truncated, full on hover) with no button, as before.
                    None => {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} \u{2014} {addr}",
                                truncate_mono(peer_id, 18)
                            ))
                            .monospace()
                            .small(),
                        )
                        .on_hover_text(peer_id);
                    }
                }
            });
        }
    }
}

/// The "Conflicts" section: list every forked (blocked) document with the peer
/// it forked against and the reason, plus per-row resolution verbs. Reuses the
/// `op-log-merge-conflict` keep-mine / keep-theirs / keep-both shape. Only
/// rendered when `blocked` is non-empty. [sync-blocked-state]
fn conflicts_section(
    ui: &mut egui::Ui,
    app: &mut AppState,
    service: &Arc<crate::sync_service::SyncService>,
    blocked: &[hiker_sync::identity::BlockedDoc],
    rt: &Arc<tokio::runtime::Runtime>,
) {
    ui.label(
        egui::RichText::new(format!("\u{26A0} Conflicts ({})", blocked.len()))
            .color(egui::Color32::from_rgb(210, 150, 40))
            .strong(),
    );
    ui.label(
        egui::RichText::new(
            "These documents forked: both devices edited them with no shared history, so \
             they can't auto-merge. Pick a side per document.",
        )
        .color(theme::muted())
        .small(),
    );
    ui.add_space(4.0);

    for doc in blocked {
        // Peer name: local alias override, else the learned synced name, else a
        // truncated fingerprint. [sync-device-name]
        let fp = &doc.peer_fingerprint.0;
        let peer = service
            .display_name(fp)
            .unwrap_or_else(|| truncate_mono(fp, 18));
        ui.group(|ui| {
            ui.label(egui::RichText::new(&doc.path).strong());
            ui.label(
                egui::RichText::new(format!("forked with {peer} \u{2014} {}", doc.reason))
                    .color(theme::muted())
                    .small(),
            )
            .on_hover_text(fp);

            ui.horizontal(|ui| {
                if ui.button("Keep mine").clicked() {
                    service.resolve_fork(&doc.path, Resolution::KeepMine, rt);
                    app.push_toast(
                        "Keeping your version — pushing it so the other device adopts it too",
                        ToastLevel::Info,
                    );
                }
                if ui.button("Keep theirs").clicked() {
                    service.resolve_fork(&doc.path, Resolution::KeepTheirs, rt);
                    app.push_toast("Adopting the peer's version", ToastLevel::Info);
                }
                if ui.button("Keep both").clicked() {
                    service.resolve_fork(&doc.path, Resolution::KeepBoth, rt);
                    app.push_toast(
                        "Saving your version as a conflict copy, then adopting theirs",
                        ToastLevel::Info,
                    );
                }

                // "View diff": fetch the peer's CURRENT text on demand and show
                // a read-only ours-vs-theirs unified diff before resolving. A
                // fork holds our version but not theirs (forks are detected from
                // hashes, never the body), so the peer must be online to diff.
                // [sync-fork-diff]
                let shown = matches!(
                    app.panels.sync.fork_diffs.get(&doc.path),
                    Some(ForkDiffState::Ready(_) | ForkDiffState::Fetching | ForkDiffState::Error(_))
                );
                let label = if shown { "Hide" } else { "View diff" };
                if ui.button(label).clicked() {
                    if shown {
                        app.panels.sync.fork_diffs.remove(&doc.path);
                    } else {
                        app.panels
                            .sync
                            .fork_diffs
                            .insert(doc.path.clone(), ForkDiffState::Fetching);
                        service.fetch_fork_diff(&doc.path, fp, rt);
                    }
                }
            });

            // Inline read-only diff, rendered per cache state.
            render_fork_diff(ui, app, &doc.path);
        });
        ui.add_space(4.0);
    }
}

/// Render the inline "view diff" surface for one forked-doc row, dispatching on
/// the path's [`ForkDiffState`] in the Sync page cache. `Ready(their_text)`
/// computes a unified diff of OUR `materialize_accepted(path).text` (base) vs
/// THEIR text (current) via `core::diff::compute` and renders it bounded +
/// colored. Lock-free on the render path: reads the UI cache and the read-side
/// op log, never the sync node. [sync-fork-diff]
fn render_fork_diff(ui: &mut egui::Ui, app: &AppState, path: &str) {
    let Some(state) = app.panels.sync.fork_diffs.get(path) else {
        return;
    };
    ui.add_space(4.0);
    match state {
        ForkDiffState::Fetching => {
            ui.label(
                egui::RichText::new("fetching peer's version\u{2026}")
                    .color(theme::muted())
                    .italics()
                    .small(),
            );
        }
        ForkDiffState::Error(msg) => {
            ui.colored_label(egui::Color32::from_rgb(200, 60, 60), msg);
        }
        ForkDiffState::Ready(their_text) => {
            // OUR base text: the local accepted materialization for the path.
            // Read off the op log (read-side), never the sync node.
            let oplog = &app.vault_session.services.oplog;
            let ours = oplog
                .doc_id_for_path(path)
                .ok()
                .flatten()
                .and_then(|id| oplog.materialize_accepted(&id).ok())
                .map(|c| c.text)
                .unwrap_or_default();
            render_unified_diff(ui, &ours, their_text);
        }
    }
}

/// Render a bounded, read-only unified diff of `ours` (base) vs `theirs`
/// (current) using `core::diff::compute`: changed lines carry `+`/`-` with
/// restrained add/remove colors, equal-context lines are muted, and the line
/// count is capped with a "…" marker so a huge fork stays bounded inside the
/// page scroll. [sync-fork-diff]
fn render_unified_diff(ui: &mut egui::Ui, ours: &str, theirs: &str) {
    use hiker_core::diff::Op;
    let outcome = hiker_core::diff::compute(ours, theirs);
    let total: usize = outcome.hunks.iter().map(|h| h.lines.len()).sum();
    if total == 0 {
        ui.label(
            egui::RichText::new("(no differences — versions match)")
                .color(theme::muted())
                .italics()
                .small(),
        );
        return;
    }
    // Restrained add/remove colors; equal lines muted.
    let add = egui::Color32::from_rgb(90, 170, 90);
    let del = egui::Color32::from_rgb(200, 90, 90);
    let mut shown = 0usize;
    'outer: for hunk in &outcome.hunks {
        for line in &hunk.lines {
            if shown >= FORK_DIFF_MAX_LINES {
                ui.label(
                    egui::RichText::new(format!(
                        "\u{2026} ({} more lines)",
                        total - shown
                    ))
                    .color(theme::muted())
                    .small(),
                );
                break 'outer;
            }
            let (prefix, color) = match line.op {
                Op::Insert => ('+', Some(add)),
                Op::Delete => ('-', Some(del)),
                Op::Equal => (' ', None),
            };
            let text = format!("{prefix} {}", line.line);
            let mut rich = egui::RichText::new(text).monospace().small();
            rich = match color {
                Some(c) => rich.color(c),
                None => rich.color(theme::muted()),
            };
            ui.label(rich);
            shown += 1;
        }
    }
}

/// The content-key section: show this vault's key (bs58) with a Copy button and
/// a secret warning, plus a paste field + Import button. The manual stand-in for
/// the in-band key handshake. [sync-vault-key-inband]
fn content_key_section(
    ui: &mut egui::Ui,
    app: &mut AppState,
    service: &Arc<crate::sync_service::SyncService>,
    rt: &Arc<tokio::runtime::Runtime>,
) {
    ui.label(egui::RichText::new("Content key").color(theme::muted()));
    ui.label(
        egui::RichText::new(
            "This is a secret — only share it with your own devices, over a trusted \
             channel. (Server-mode key mismatches can't be auto-detected; importing the \
             same key on every device is the fix.)",
        )
        .color(egui::Color32::from_rgb(180, 140, 60))
        .small()
        .italics(),
    );
    // Cheap snapshot read — never lock the node on the render path.
    let key_b58 = service.state_snapshot().content_key_b58;
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(truncate_mono(&key_b58, 24))
                .monospace()
                .small(),
        )
        .on_hover_text(&key_b58);
        if ui.small_button("Copy").clicked() {
            ui.ctx().copy_text(key_b58.clone());
            app.push_toast("Content key copied", ToastLevel::Info);
        }
    });

    let import_id = egui::Id::new("sync-content-key-import-draft");
    let mut draft: String = ui
        .ctx()
        .data(|d| d.get_temp::<String>(import_id))
        .unwrap_or_default();
    let mut do_import = false;
    ui.horizontal(|ui| {
        let resp = ui.add(
            egui::TextEdit::singleline(&mut draft)
                .hint_text("paste a content key to import")
                .desired_width(360.0)
                .password(true),
        );
        let submit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ui.button("Import").clicked() || submit {
            do_import = true;
        }
    });
    if do_import {
        let pasted = draft.trim().to_string();
        match service.import_content_key(&pasted, rt) {
            Ok(()) => {
                app.push_toast("Content key imported", ToastLevel::Info);
                draft.clear();
            }
            Err(e) => {
                app.push_toast(format!("Import failed: {e}"), ToastLevel::Error);
            }
        }
    }
    ui.ctx().data_mut(|d| d.insert_temp(import_id, draft));
}

/// The enrolled-device rows: each device's alias (or truncated fingerprint with
/// a full-fp tooltip), an inline rename field, and a Remove button.
fn enrolled_devices_section(
    ui: &mut egui::Ui,
    app: &mut AppState,
    service: &Arc<crate::sync_service::SyncService>,
    rt: &Arc<tokio::runtime::Runtime>,
) {
    let enrolled = service.enrolled_devices();
    if enrolled.is_empty() {
        ui.label(
            egui::RichText::new("(none enrolled yet)")
                .color(theme::muted())
                .italics()
                .small(),
        );
        return;
    }
    for fp in &enrolled {
        let alias = service.device_alias(fp);
        let learned = service.learned_name(fp);
        ui.horizontal(|ui| {
            // Display precedence: local alias override, else the peer's learned
            // self-reported synced name, else a truncated fingerprint. Full fp on
            // hover either way. [sync-device-name]
            let display = match (&alias, &learned) {
                (Some(name), _) => name.clone(),
                (None, Some(name)) => name.clone(),
                (None, None) => truncate_mono(fp, 18),
            };
            ui.label(egui::RichText::new(display).monospace().small())
                .on_hover_text(fp);

            // Inline rename field for the LOCAL alias override (wins over the
            // synced name). Seeded with the current alias. [sync-device-name]
            let name_id = egui::Id::new(("sync-alias", fp.clone()));
            let mut name: String = ui
                .ctx()
                .data(|d| d.get_temp::<String>(name_id))
                .unwrap_or_else(|| alias.clone().unwrap_or_default());
            let resp = ui.add(
                egui::TextEdit::singleline(&mut name)
                    .hint_text("name")
                    .desired_width(120.0),
            );
            let submit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.small_button("Rename").clicked() || submit {
                service.set_alias(fp, name.trim());
                app.push_toast("Device renamed", ToastLevel::Info);
            }
            ui.ctx().data_mut(|d| d.insert_temp(name_id, name));

            if ui.small_button("Remove").clicked() {
                match service.unenroll_device(&app.vault_session.vault_root, fp, rt) {
                    Ok(()) => app.push_toast("Device removed", ToastLevel::Info),
                    Err(e) => {
                        app.push_toast(format!("Remove failed: {e}"), ToastLevel::Error)
                    }
                }
            }
        });
    }
}

/// Truncate a long monospace string to `max` chars with an ellipsis, for the
/// fingerprint / content-key display (the full value lives in a hover tooltip).
fn truncate_mono(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}\u{2026}")
    }
}

/// Render one recently-synced op as a line, e.g.
/// `Modified notes/foo.md — synced from phone`.
fn synced_item_line(item: &hiker_core::activity::Item) -> String {
    let verb = match &item.summary {
        Summary::Change { op } => match op {
            hiker_core::activity::ChangeOp::Created => "Created",
            hiker_core::activity::ChangeOp::Modified => "Modified",
            hiker_core::activity::ChangeOp::Deleted => "Deleted",
            hiker_core::activity::ChangeOp::Renamed => "Renamed",
        },
        Summary::Pending { .. } => "Pending",
    };
    // The wire form of a sync author is `sync:<device>`; surface the device.
    let device = item
        .author
        .split_once(':')
        .map(|(_, dev)| dev)
        .filter(|d| !d.is_empty());
    match device {
        Some(dev) => format!("{verb} {} — synced from {dev}", item.path),
        None => format!("{verb} {} — synced", item.path),
    }
}

/// Human-readable last-sync summary: "<n>s ago — 2 converged, 0 blocked".
fn format_last_sync(
    ms: i64,
    report: Option<&hiker_sync::transport::SyncReport>,
) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(ms);
    let secs = ((now - ms).max(0) / 1000) as u64;
    let ago = if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    };
    match report {
        Some(r) => format!(
            "{ago} — {} converged, {} blocked",
            r.converged.len(),
            r.blocked.len()
        ),
        None => ago,
    }
}
