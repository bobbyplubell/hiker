//! WARC assembly for the CEF capture path (`crawler-warc-archive`).
//!
//! The CEF engine taps the page's wire responses through CDP's `Network`
//! domain (`Network.responseReceived` for metadata, `Network.getResponseBody`
//! for bodies) and accumulates them as [`CapturedResponse`] entries. This
//! module turns those entries into a standards-shaped WARC byte stream — a
//! `warcinfo` record followed by one `response` record per captured resource —
//! using the pure-Rust `warc` crate. The result is the higher-fidelity capture
//! format the spec parks for the crawler (actual wire responses, not a DOM
//! re-serialization), and it flows through `manifest::Page.archive_file`
//! unchanged.
//!
//! WARC `response` records carry the full HTTP response (status line, headers,
//! and body) as their block. We synthesize a minimal but valid HTTP/1.1
//! response head from the CDP metadata; CDP does not hand back the raw status
//! line, so a reconstructed one is the faithful-as-possible stand-in.

use std::io::BufWriter;

use warc::{BufferedBody, Record, RecordBuilder, RecordType, WarcHeader, WarcWriter};

/// One response captured off the wire, assembled from the CDP `Network` events
/// for a single request id. Bodies that never arrived (eviction raced the
/// `getResponseBody` round-trip) leave [`Self::body`] empty.
#[derive(Debug, Clone, Default)]
pub struct CapturedResponse {
    /// The resource URL (`Network.responseReceived` → `response.url`).
    pub url: String,
    /// HTTP status code (`response.status`).
    pub status: u32,
    /// HTTP status text (`response.statusText`), e.g. `OK`.
    pub status_text: String,
    /// Response header name/value pairs (`response.headers`), in arrival order
    /// where CDP preserves it.
    pub headers: Vec<(String, String)>,
    /// The MIME type CDP resolved (`response.mimeType`).
    pub mime_type: String,
    /// The response body bytes. Empty when the body was evicted before
    /// `Network.getResponseBody` returned.
    pub body: Vec<u8>,
}

/// Assemble `responses` into a single WARC byte stream: a `warcinfo` record
/// describing the capture, then one `response` record per entry. `page_url` is
/// the navigated page (recorded in the `warcinfo` block for provenance).
///
/// Entries with an empty [`CapturedResponse::url`] are skipped (nothing to key
/// a `WARC-Target-URI` on). Returns `None` only if no record could be written
/// at all (so the caller can fall back to no archive).
// status: crawler-warc-archive
#[must_use]
pub fn assemble(page_url: &str, responses: &[CapturedResponse]) -> Option<Vec<u8>> {
    // Wrap a `Vec<u8>` in a `BufWriter` so `into_inner` (only defined for the
    // buffered writer) hands the assembled bytes back.
    let mut writer = WarcWriter::new(BufWriter::new(Vec::new()));

    let info = warcinfo_record(page_url);
    writer.write(&info).ok()?;

    let mut wrote_any = false;
    for resp in responses {
        if resp.url.is_empty() {
            continue;
        }
        if let Some(record) = response_record(resp) {
            if writer.write(&record).is_ok() {
                wrote_any = true;
            }
        }
    }

    if !wrote_any {
        return None;
    }
    writer.into_inner().ok()
}

/// The leading `warcinfo` record: a small block of `field: value` lines naming
/// the producer and the captured page, per the WARC `warcinfo` convention.
fn warcinfo_record(page_url: &str) -> Record<BufferedBody> {
    let block = format!(
        "software: hiker-crawler/{version}\r\n\
         format: WARC File Format 1.1\r\n\
         page-url: {page_url}\r\n",
        version = env!("CARGO_PKG_VERSION"),
    );
    let body = block.into_bytes();
    RecordBuilder::default()
        .warc_type(RecordType::WarcInfo)
        .header(WarcHeader::ContentType, "application/warc-fields")
        .header(WarcHeader::Filename, "page-0.warc")
        .body(body)
        .build()
        .unwrap_or_default()
}

/// One `response` record: the reconstructed HTTP response (status line +
/// headers + body) as the WARC block, tagged with the resource URL and content
/// type. Returns `None` if the record fails to build (a malformed header).
fn response_record(resp: &CapturedResponse) -> Option<Record<BufferedBody>> {
    let body = http_response_block(resp);
    RecordBuilder::default()
        .warc_type(RecordType::Response)
        .header(WarcHeader::TargetURI, resp.url.as_str())
        .header(WarcHeader::ContentType, "application/http; msgtype=response")
        .body(body)
        .build()
        .ok()
}

/// Reconstruct the HTTP/1.1 response head (`HTTP/1.1 <status> <text>` + the
/// captured headers) followed by the body, CRLF-framed — the block payload a
/// WARC `response` record carries.
fn http_response_block(resp: &CapturedResponse) -> Vec<u8> {
    let status_text = if resp.status_text.is_empty() {
        reason_phrase(resp.status)
    } else {
        resp.status_text.as_str()
    };
    let mut head = format!("HTTP/1.1 {} {}\r\n", resp.status, status_text);
    let mut had_content_type = false;
    for (name, value) in &resp.headers {
        // Skip a stale content-length: the captured body is authoritative and a
        // mismatched one would corrupt a faithful replay.
        if name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        if name.eq_ignore_ascii_case("content-type") {
            had_content_type = true;
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    // If the wire headers lacked a content-type, fall back to the MIME type CDP
    // resolved, so a faithful replay still knows how to render the body.
    if !had_content_type && !resp.mime_type.is_empty() {
        head.push_str(&format!("content-type: {}\r\n", resp.mime_type));
    }
    head.push_str(&format!("content-length: {}\r\n", resp.body.len()));
    head.push_str("\r\n");

    let mut block = head.into_bytes();
    block.extend_from_slice(&resp.body);
    block
}

/// A best-effort reason phrase for a status code when CDP didn't supply one.
const fn reason_phrase(status: u32) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::{CapturedResponse, assemble};

    fn resp(url: &str, status: u32, body: &[u8]) -> CapturedResponse {
        CapturedResponse {
            url: url.to_owned(),
            status,
            status_text: String::new(),
            headers: vec![("content-type".to_owned(), "text/html".to_owned())],
            mime_type: "text/html".to_owned(),
            body: body.to_vec(),
        }
    }

    #[test]
    fn assemble_writes_warcinfo_then_one_response_record_each() {
        let responses = vec![
            resp("https://example.com/", 200, b"<html>hi</html>"),
            resp("https://example.com/app.css", 200, b"body{}"),
        ];
        let bytes = assemble("https://example.com/", &responses).expect("warc bytes");
        let text = String::from_utf8_lossy(&bytes);
        // The warcinfo record (our provenance block) + the captured resources.
        assert!(text.contains("WARC/"));
        assert!(text.contains("software: hiker-crawler/"));
        assert!(text.contains("format: WARC File Format 1.1"));
        assert!(text.contains("https://example.com/app.css"));
        assert!(text.contains("<html>hi</html>"));
        // A reconstructed HTTP head: blank statusText falls back to a reason phrase.
        assert!(text.contains("HTTP/1.1 200 OK"));
    }

    #[test]
    fn assemble_skips_urlless_entries_and_is_none_when_nothing_to_write() {
        // An entry with no URL has nothing to key a WARC-Target-URI on → skipped,
        // and with no real records written `assemble` reports `None`.
        let nameless = vec![resp("", 200, b"x")];
        assert!(assemble("https://example.com/", &nameless).is_none());
        assert!(assemble("https://example.com/", &[]).is_none());
    }
}
