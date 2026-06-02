//! App-side wiring for the decoupled `hiker-extract` crate. The crate owns
//! the registry + sidecar write; this module is the *trigger* seam — it turns
//! a user gesture (filetree "Make searchable", or an OS-handler "open
//! original") into a call into the crate, and feeds the produced sidecar `.md`
//! back into core's ordinary indexer path. `core` never links the extraction
//! crate; the sidecar on disk is the only contract between them. See
//! `docs/extract.md` "Ingest trigger" / "Viewing: open the original
//! externally".
//
// status: extract-trigger-on-demand
// status: extract-trigger-default-ignore
// status: extract-open-original-external

use std::path::Path;

use hiker_extract::sidecar::{Producer, Writer};
use hiker_extract::{Ctx, Registry, Source};

use crate::state::{AppState, ToastLevel};

/// On-demand "Make searchable": extract a single non-markdown file now and
/// write its `.md` sidecar beside it, then enqueue the sidecar for indexing.
/// This is the explicit per-file trigger for sources outside every auto-glob
/// (`extract-trigger-on-demand`).
///
/// status: extract-trigger-on-demand
pub fn make_searchable(app: &mut AppState, rel: &str) {
    let abs = match app.vault_session.vault.abs_path(rel) {
        Ok(p) => p,
        Err(err) => {
            app.push_toast(format!("Make searchable failed: {err}"), ToastLevel::Error);
            return;
        }
    };
    let bytes = match std::fs::read(&abs) {
        Ok(b) => b,
        Err(err) => {
            app.push_toast(format!("Make searchable: read {rel}: {err}"), ToastLevel::Error);
            return;
        }
    };

    let registry = Registry::with_builtins();
    let source = Source::File(abs.clone());
    let routed = match registry.extract(&source, &Ctx::default()) {
        Ok(Some(r)) => r,
        Ok(None) => {
            app.push_toast(
                format!("No extractor handles {rel} yet"),
                ToastLevel::Info,
            );
            return;
        }
        Err(err) => {
            app.push_toast(format!("Extract {rel} failed: {err}"), ToastLevel::Error);
            return;
        }
    };

    // Re-extraction of an EXISTING linked sidecar lands as an `extractor` op on
    // its `accepted` state (`extract-version-oplog`) instead of a blind
    // overwrite — so prior bodies stay in op-log history. Only a brand-new
    // sidecar (no op-log doc yet) takes the direct `Writer` path below.
    let sidecar_rel = format!("{rel}.md");
    let already_extracted = app
        .vault_session
        .services
        .oplog
        .doc_id_for_path(&sidecar_rel)
        .ok()
        .flatten()
        .is_some();
    if already_extracted {
        reextract_existing(app, &sidecar_rel, &routed.extracted.markdown, &routed.extractor_name);
        return;
    }

    let clip_folder = app
        .vault_session
        .config
        .read()
        .map(|c| c.extract.clip_folder.clone())
        .unwrap_or_else(|_| "clips/".to_string());
    let writer = Writer::new(&app.vault_session.vault_root, clip_folder);
    let source_type = source.extension().unwrap_or_else(|| "file".to_string());
    let mtime_iso = file_mtime_iso(&abs);
    // Provenance label: the extractor name doubles as the specific
    // provenance for the built-in set (`pdf`, `web-scrape`, …).
    let producer = Producer {
        extractor_name: &routed.extractor_name,
        extractor_version: &routed.extractor_version,
        provenance: &routed.extractor_name,
    };
    let written = writer.write_file_sidecar(&abs, &bytes, &mtime_iso, &routed.extracted, &producer, &source_type);
    match written {
        Ok(w) => {
            enqueue_index(app, &w.path);
            app.push_toast(format!("Extracted {rel}"), ToastLevel::Info);
        }
        Err(err) => {
            app.push_toast(format!("Write sidecar for {rel}: {err}"), ToastLevel::Error);
        }
    }
}

/// Route a re-extraction of an existing sidecar through the op-log: replace the
/// linked body in place as an `extractor` op (or skip when the user unlinked).
/// The op-log atomically rewrites the `.md`, so no separate write happens; the
/// sidecar re-enters indexing on the resulting file change. An identical
/// re-extraction is a silent no-op (no version, no toast churn).
///
/// status: extract-version-oplog
fn reextract_existing(app: &mut AppState, sidecar_rel: &str, new_body: &str, extractor_id: &str) {
    use hiker_core::ops::op_writes::{self, ReextractOutcome};
    let oplog = app.vault_session.services.oplog.clone();
    let vault = app.vault_session.vault.clone();
    match op_writes::reextract(&oplog, &vault, sidecar_rel, new_body, extractor_id) {
        Ok(ReextractOutcome::Replaced) => {
            if let Ok(abs) = vault.abs_path(sidecar_rel) {
                enqueue_index(app, &abs);
            }
            app.push_toast(format!("Re-extracted {sidecar_rel}"), ToastLevel::Info);
        }
        Ok(ReextractOutcome::Unchanged) => {}
        Ok(ReextractOutcome::Skipped) => {
            app.push_toast(
                format!("{sidecar_rel} is unlinked — re-extraction skipped"),
                ToastLevel::Info,
            );
        }
        Err(err) => {
            app.push_toast(format!("Re-extract {sidecar_rel}: {err}"), ToastLevel::Error);
        }
    }
}

/// The "view original opens in the OS handler" seam: hand an absolute source
/// path to the platform's default app. Reused for the default-ignore path
/// (an unsupported file just opens externally) and the "view original"
/// action on a sidecar/source. No in-app web/PDF renderer.
///
/// status: extract-open-original-external
pub fn open_external(app: &mut AppState, rel: &str) {
    let abs = match app.vault_session.vault.abs_path(rel) {
        Ok(p) => p,
        Err(err) => {
            app.push_toast(format!("Open failed: {err}"), ToastLevel::Error);
            return;
        }
    };
    if let Err(err) = launch_os_handler(abs.as_os_str()) {
        app.push_toast(format!("Open failed: {err}"), ToastLevel::Error);
    }
}

/// Hand an external URL (`http(s)`/`mailto:`) to the OS default handler — the
/// browser / mail client. Shares the same cross-platform spawn as the
/// open-original path; the only difference is the argument is a URL string
/// rather than an absolute file path. Used by interactive surfaces (e.g. a
/// mermaid diagram `click` directive resolving to an `External` link).
/// status: widget-mermaid-links
pub fn open_external_url(app: &mut AppState, url: &str) {
    if let Err(err) = launch_os_handler(url.as_ref()) {
        app.push_toast(format!("Open failed: {err}"), ToastLevel::Error);
    }
}

/// Best-effort cross-platform "open in the default app" (xdg-open /
/// equivalent). Distinct from `reveal_in_file_manager` (which selects the
/// file in a folder window); this opens the file itself. Accepts any
/// `&OsStr`-able target — an absolute file path or an external URL string.
fn launch_os_handler(arg: &std::ffi::OsStr) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(arg);
    cmd.spawn().map(|_| ())
}

/// Enqueue the freshly-written sidecar for indexing via the existing indexer
/// job channel — the same path `core`'s watcher uses, so the sidecar enters
/// the ordinary markdown ingest flow.
fn enqueue_index(app: &mut AppState, sidecar_abs: &Path) {
    let Ok(rel) = app
        .vault_session
        .vault
        .abs_path("")
        .map(|root| {
            sidecar_abs
                .strip_prefix(&root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default()
        })
    else {
        return;
    };
    if rel.is_empty() {
        return;
    }
    let tx = app.vault_session.services.indexer.job_sender();
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            let _ = tx
                .send(hiker_core::indexer::IndexJob::Upsert { rel_path: rel, force: false })
                .await;
        });
    }
}

/// ISO-8601 mtime of `abs`, or `"unknown"` if it can't be read. Stamped onto
/// the sidecar's `hiker.source_mtime`.
fn file_mtime_iso(abs: &Path) -> String {
    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;
    std::fs::metadata(abs)
        .and_then(|m| m.modified())
        .map(OffsetDateTime::from)
        .ok()
        .and_then(|dt| dt.format(&Rfc3339).ok())
        .unwrap_or_else(|| "unknown".to_string())
}
