//! Hand-rolled, blocking, single-threaded JSON-RPC-over-stdio client for an LSP server.
//!
//! No async runtime, no `lsp-types`, no `tower-lsp` — just `std::process` + `serde_json`. The read
//! loop ([`LspClient::read_until`]) handles the three LSP message shapes:
//!   1. **responses** (`id` + `result`/`error`) — matched to the pending request id;
//!   2. **server notifications** (`method`, no `id`) — e.g. `$/progress`, drained/ignored;
//!   3. **server→client requests** (`method` + `id`) — e.g. `window/workDoneProgress/create`,
//!      `client/registerCapability` — which we MUST answer (`result: null`) or RA stalls.
//!
//! `request` blocks reading framed `Content-Length` messages, replying to any server requests and
//! recording the latest `$/progress` `end` token, until the matching response id arrives. `notify`
//! is fire-and-forget. Stderr is drained on a background thread so RA's chatter never blocks us.

use std::collections::HashSet;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

/// A live LSP server process plus its framed stdio channel.
pub struct LspClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    /// URIs already sent via `textDocument/didOpen` (positional requests require an open document).
    opened: HashSet<String>,
    /// Repo root, for clamping `didOpen` file reads under it.
    root: PathBuf,
    /// Whether we've seen a `$/progress` `end` notification since the last drain (ready-wait hint).
    saw_progress_end: bool,
    shutdown_sent: bool,
}

/// Resolve a `file://` URI to a repo-relative path, refusing absolute/`..`/root traversal so a
/// server can never make us read outside `root`. Mirrors `scip_adapter::safe_join`'s contract.
pub fn safe_join(root: &Path, file: &str) -> Option<PathBuf> {
    use std::path::Component;
    let rel = Path::new(file);
    if rel
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
    {
        return None;
    }
    let joined = root.join(rel);
    if let (Ok(r), Ok(j)) = (root.canonicalize(), joined.canonicalize()) {
        if !j.starts_with(&r) {
            return None;
        }
    }
    Some(joined)
}

impl LspClient {
    /// Spawn `program` (rust-analyzer) with piped stdin/stdout in `root`, draining stderr on a
    /// background thread. The returned client is not yet initialized — call the lifecycle helpers.
    pub fn spawn(program: &Path, root: &Path) -> io::Result<LspClient> {
        let mut child = Command::new(program)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| io_err("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| io_err("no stdout"))?;
        if let Some(stderr) = child.stderr.take() {
            let debug = std::env::var("HIKER_LSP_DEBUG").is_ok();
            std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    if debug {
                        eprintln!("[ra] {line}");
                    }
                }
            });
        }
        Ok(LspClient {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            opened: HashSet::new(),
            root: root.to_path_buf(),
            saw_progress_end: false,
            shutdown_sent: false,
        })
    }

    /// Frame and write a single JSON-RPC message with a `Content-Length` header.
    fn write_message(&mut self, msg: &Value) -> io::Result<()> {
        let body = serde_json::to_vec(msg)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())?;
        self.stdin.write_all(&body)?;
        self.stdin.flush()
    }

    /// Read one framed message: parse `Content-Length`, skip other headers, read exactly N bytes.
    fn read_message(&mut self) -> io::Result<Value> {
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line)?;
            if n == 0 {
                return Err(io_err("server closed stdout"));
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break; // end of headers
            }
            if let Some(v) = trimmed.strip_prefix("Content-Length:") {
                content_length = v.trim().parse().ok();
            }
        }
        let len = content_length.ok_or_else(|| io_err("missing Content-Length"))?;
        let mut body = vec![0u8; len];
        self.stdout.read_exact(&mut body)?;
        serde_json::from_slice(&body).map_err(|e| io_err(&format!("bad json: {e}")))
    }

    /// Fire-and-forget notification (no response expected).
    pub fn notify(&mut self, method: &str, params: Value) -> io::Result<()> {
        self.write_message(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// Issue a request and block until its response arrives, servicing server→client requests and
    /// draining notifications in between. Returns the `result` value (or an error on `error`).
    pub fn request(&mut self, method: &str, params: Value) -> io::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        if std::env::var("HIKER_LSP_DEBUG").is_ok() {
            eprintln!("[req {id}] {method}");
        }
        self.write_message(&json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }))?;
        let r = self.read_until(id);
        if std::env::var("HIKER_LSP_DEBUG").is_ok() {
            eprintln!("[req {id}] {method} -> {}", if r.is_ok() { "ok" } else { "ERR" });
        }
        r
    }

    /// Pump messages until the response for `want_id` arrives. Replies to server requests with
    /// `result: null`, notes `$/progress` `end` tokens, and ignores other notifications.
    fn read_until(&mut self, want_id: i64) -> io::Result<Value> {
        loop {
            let msg = self.read_message()?;
            // Server → client request: has both `method` and `id` → must reply or RA stalls.
            if msg.get("method").is_some() && msg.get("id").is_some() {
                let rid = msg.get("id").cloned().unwrap_or(Value::Null);
                self.write_message(&json!({ "jsonrpc": "2.0", "id": rid, "result": Value::Null }))?;
                continue;
            }
            // Notification: `method`, no `id`.
            if msg.get("method").is_some() {
                if msg.get("method").and_then(Value::as_str) == Some("$/progress")
                    && progress_is_end(&msg)
                {
                    self.saw_progress_end = true;
                }
                continue;
            }
            // Response: `id` + `result`/`error`.
            if msg.get("id").and_then(Value::as_i64) == Some(want_id) {
                if let Some(err) = msg.get("error") {
                    return Err(io_err(&format!("lsp error: {err}")));
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
            // A stale/other response id — ignore and keep reading.
        }
    }

    /// True (and resets the flag) if a `$/progress` `end` was seen since the last check. The
    /// ready-wait loop reads this between `workspace/symbol` probes to shorten its backoff (the
    /// `read_until` driving each probe is what actually drains the progress stream + sets the flag).
    pub fn take_progress_end(&mut self) -> bool {
        std::mem::replace(&mut self.saw_progress_end, false)
    }

    /// Ensure `uri` has been `didOpen`ed before a positional request. Reads the file via
    /// [`safe_join`] (clamped under the repo root) and sends `textDocument/didOpen` once.
    pub fn ensure_open(&mut self, uri: &str) -> io::Result<()> {
        if self.opened.contains(uri) {
            return Ok(());
        }
        let rel = uri.strip_prefix("file://").unwrap_or(uri);
        let abs = strip_root(&self.root, rel);
        let text = match safe_join(&self.root, &abs) {
            Some(p) => std::fs::read_to_string(p).unwrap_or_default(),
            None => String::new(),
        };
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": uri, "languageId": "rust", "version": 1, "text": text
            }}),
        )?;
        self.opened.insert(uri.to_string());
        Ok(())
    }

    /// Best-effort `shutdown` + `exit`, then kill the child if it lingers. Idempotent.
    pub fn close(&mut self) {
        if self.shutdown_sent {
            return;
        }
        self.shutdown_sent = true;
        let _ = self.request("shutdown", Value::Null);
        let _ = self.notify("exit", Value::Null);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.close();
    }
}

/// Turn an absolute path (from a `file://` uri) into a repo-root-relative path when it lives under
/// `root`; otherwise return it unchanged (so `safe_join` can reject it).
fn strip_root(root: &Path, abs: &str) -> String {
    let abs_path = Path::new(abs);
    let stripped = match (root.canonicalize(), abs_path.canonicalize()) {
        (Ok(r), Ok(a)) => a.strip_prefix(&r).ok().map(|p| p.to_path_buf()),
        _ => abs_path.strip_prefix(root).ok().map(|p| p.to_path_buf()),
    };
    stripped
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| abs.to_string())
}

/// True if `msg` is a `$/progress` notification whose value `kind` is `"end"`.
fn progress_is_end(msg: &Value) -> bool {
    msg.get("params")
        .and_then(|p| p.get("value"))
        .and_then(|v| v.get("kind"))
        .and_then(Value::as_str)
        == Some("end")
}

fn io_err(msg: &str) -> io::Error {
    io::Error::other(msg.to_string())
}
