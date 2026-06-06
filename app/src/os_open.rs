//! Hand a file or URL to the platform's default application. Generic
//! OS-handler launch, independent of any content subsystem: used by the
//! filetree "Open original externally" action (a source the app has no in-app
//! renderer for) and by interactive surfaces that resolve an external link
//! (e.g. a mermaid diagram `click` directive). Distinct from
//! `reveal_in_file_manager`, which selects a file in a folder window rather
//! than opening it.
//
// status: extract-open-original-external
// status: widget-mermaid-links

use crate::state::{AppState, ToastLevel};

/// Open an in-vault file in the OS default app. `rel` is vault-relative; it
/// resolves to an absolute path before launch.
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
/// rather than an absolute file path.
/// status: widget-mermaid-links
pub fn open_external_url(app: &mut AppState, url: &str) {
    if let Err(err) = launch_os_handler(url.as_ref()) {
        app.push_toast(format!("Open failed: {err}"), ToastLevel::Error);
    }
}

/// Best-effort cross-platform "open in the default app" (xdg-open /
/// equivalent). Accepts any `&OsStr`-able target — an absolute file path or an
/// external URL string.
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
