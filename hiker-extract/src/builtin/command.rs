//! Command-driven extraction logic behind [`crate::builtin::CommandExtractor`]
//! — the per-glob escape hatch (`extract-pdf-command-escape`, design.md
//! "Generic escape hatch — CommandExtractor").
//!
//! Shells out to a **user-configured** command — `pdftotext`, `marker`,
//! `docling`, `epub2txt`, … — to turn a source into text without anyone writing
//! Rust. The mechanism is format-agnostic; PDF is just its first user (someone
//! who wants higher-fidelity PDF extraction today, before the native
//! marker/docling fallback lands, wires `pdftotext` through it).
//!
//! `{input}` in the command template expands to the absolute source path;
//! `{output}`, when present, expands to a temp file the command writes and
//! whose contents become the body. A template with no `{output}` placeholder
//! captures the command's stdout instead.
//!
//! SECURITY POSTURE: the command and its arguments come from the user's own
//! vault config — never from the network, an agent, or the source bytes. This
//! runs a user-chosen binary on the user's machine, exactly as if they had run
//! it from a shell. Hiker passes only the source path (as a single argv entry,
//! never shell-interpolated) and an output temp path; it does not build a shell
//! string, so source filenames can't inject arguments. Same trust level as any
//! CLI tool the user installs and points hiker at; deliberately NOT reachable
//! from agent- or net-supplied input.
//
// status: extract-pdf-command-escape

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::ExtractError;

use super::CommandExtractor;

/// Run the configured command for `source_path` and return its text output —
/// or `Ok(None)` when the command produced only whitespace (so the fallback
/// chain, or the skipped-state recorder, can take over, same contract as the
/// PDF fast path's scanned-detect). A non-zero exit, a missing binary, or
/// non-UTF-8 output is a hard `Err`.
pub(super) fn run(spec: &CommandExtractor, source_path: &Path) -> Result<Option<String>, ExtractError> {
    let Some((program, arg_templates)) = spec.command.split_first() else {
        return Err(spec_err(spec, "empty command template"));
    };

    // A temp file for the `{output}` placeholder, dropped at scope end. Created
    // even when the command captures stdout (cheap, keeps the path uniform).
    let tmp = TempOutput::create(source_path)?;
    let args: Vec<String> = arg_templates
        .iter()
        .map(|a| expand(a, source_path, tmp.path()))
        .collect();

    let output = Command::new(program)
        .args(&args)
        .output()
        .map_err(|e| spec_err(spec, &format!("spawn `{program}`: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(spec_err(
            spec,
            &format!("`{program}` exited with {}: {}", output.status, stderr.trim()),
        ));
    }

    let text = if uses_output_file(spec) {
        std::fs::read_to_string(tmp.path())
            .map_err(|e| spec_err(spec, &format!("read command output file: {e}")))?
    } else {
        String::from_utf8(output.stdout)
            .map_err(|_| spec_err(spec, "command stdout was not valid UTF-8 text"))?
    };

    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(text))
}

/// Whether the command template writes to an `{output}` file (vs. emitting the
/// body on stdout).
fn uses_output_file(spec: &CommandExtractor) -> bool {
    spec.command.iter().any(|a| a.contains("{output}"))
}

/// Substitute the `{input}` / `{output}` placeholders in one argument template.
/// Each placeholder expands to a path; the result is a single argv entry, so a
/// path containing spaces or shell metacharacters is one argument, never
/// re-split or interpreted.
fn expand(template: &str, input: &Path, output: &Path) -> String {
    template
        .replace("{input}", &input.to_string_lossy())
        .replace("{output}", &output.to_string_lossy())
}

/// Wrap a message as the extractor's hard error, tagged with its name.
fn spec_err(spec: &CommandExtractor, msg: &str) -> ExtractError {
    ExtractError::Extractor(spec.name.clone(), msg.to_string())
}

/// An RAII temp-file handle for the `{output}` target, cleaned up on drop.
struct TempOutput {
    path: PathBuf,
}

impl TempOutput {
    /// Create a unique empty temp file for the command's `{output}`, in the
    /// system temp dir, named off the source stem so a user inspecting it can
    /// tell what it came from.
    fn create(source: &Path) -> Result<Self, ExtractError> {
        let stem = source
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "source".into());
        let unique = ulid::Ulid::new();
        let path = std::env::temp_dir().join(format!("hiker-extract-{stem}-{unique}.txt"));
        std::fs::write(&path, b"")
            .map_err(|e| ExtractError::Io(format!("create command output temp file: {e}")))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempOutput {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
