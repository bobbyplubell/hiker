//! Discovery file: `vault/.hiker/mcp.json` carries the URL agents should
//! connect to. Written on bind, removed on shutdown.
//
// status: mcp-port-discovery

use std::fs;
use std::path::Path;

use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Serialize)]
struct DiscoveryFile<'a> {
    url: &'a str,
    version: &'a str,
    started_at: String,
    vault_root: String,
}

pub fn write(path: &Path, url: &str, vault_root: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let started_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string());
    let payload = DiscoveryFile {
        url,
        version: "1",
        started_at,
        vault_root: vault_root.to_string_lossy().into_owned(),
    };
    let bytes = serde_json::to_vec_pretty(&payload)
        .map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}
