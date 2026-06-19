//! ZIM-consumer adapter: a **lazy** [`DerivedNodeSource`] over a `.zim` archive via the `zxr` ZIM
//! reader. Proves a 2nd, non-code source behind the spec-engine port (the 1st is `hiker-code`'s
//! `ScipAdapter`). See `code-in-hiker-scratch.md` ("Item 4 decision"): a big Wikipedia ZIM is a
//! local-but-huge artifact, so it wants **lazy navigation** — `resolve` an article via the title
//! index, then parse ONLY that one article's hyperlinks on `neighbors`. There is deliberately no
//! whole-graph accessor; the archive is never materialized.
//!
//! Identity: a node's [`NodeHandle::id`] is the article's **content URL** (what `title_search` and
//! `resolve_href` yield). [`SourceLoc`] is `C/<id>` with zeroed lines (ZIM has no line model).
//!
//! UI-free: depends only on `spec-engine` + `zxr` — never hiker-core or any UI crate. The
//! `resolve_href`/`scan_hrefs`/`percent_decode` helpers are carried here (with attribution) because
//! they're private in `app/src/panels/zim.rs` + `zxr/src/main.rs`; the `resolve_href` external-link
//! check is a plain scheme test (no `hiker_core::url::classify`).

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::Path;

use spec_engine::{
    DerivedNodeSource, EdgeKind, Fingerprint, NodeHandle, SourceCaps, SourceId, SourceLoc,
};
use zxr::zim::Zim;

/// A read-only, lazy [`DerivedNodeSource`] over an opened ZIM archive.
///
/// The archive is memmap-backed (`Send + Sync`); every method touches only the entries it needs —
/// `resolve`/`locate` hit the title index, `content` reads one article's cluster, and `neighbors`
/// parses exactly one article's HTML. Nothing walks the whole archive.
pub struct ZimAdapter {
    archive: Zim,
    source: SourceId,
}

impl ZimAdapter {
    /// Open the `.zim` at `path` as a derived source identified by `source`. The archive is
    /// memory-mapped, not loaded — open is cheap regardless of archive size.
    pub fn open(path: &Path, source: SourceId) -> io::Result<Self> {
        let archive = Zim::open(path)?;
        Ok(Self { archive, source })
    }

    fn handle(&self, id: &str) -> NodeHandle {
        NodeHandle { source: self.source.clone(), id: id.to_string() }
    }

    /// Read an article's HTML by content URL, trying the modern `C` namespace then the legacy `A`
    /// namespace. Mirrors `app/src/panels/zim.rs::content_article`. Follows redirects (in `zxr`).
    fn read_article(&self, id: &str) -> Option<String> {
        let bytes = self
            .archive
            .article_by_url(b'C', id)
            .or_else(|| self.archive.article_by_url(b'A', id))?;
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
}

impl DerivedNodeSource for ZimAdapter {
    fn resolve(&self, query: &str, scope: &SourceId) -> Option<NodeHandle> {
        if scope != &self.source {
            return None;
        }
        // Title-index binary search; take the single best (first) prefix hit's content url.
        let (_title, url) = self.archive.title_search(query, 1).into_iter().next()?;
        Some(self.handle(&url))
    }

    fn locate(&self, h: &NodeHandle) -> Option<SourceLoc> {
        // ZIM has no line model: a node "lives" at its content-namespace entry path.
        Some(SourceLoc { file: format!("C/{}", h.id), start_line: 0, end_line: 0 })
    }

    fn content(&self, h: &NodeHandle) -> Option<String> {
        self.read_article(&h.id)
    }

    fn fingerprint(&self, h: &NodeHandle) -> Option<Fingerprint> {
        // Mirror ScipAdapter's normalization/format: trim trailing whitespace per line, hash, hex.
        let content = self.content(h)?;
        let norm = content.lines().map(str::trim_end).collect::<Vec<_>>().join("\n");
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        norm.hash(&mut hasher);
        Some(Fingerprint(format!("{:016x}", hasher.finish())))
    }

    fn neighbors(&self, h: &NodeHandle, kinds: &[EdgeKind]) -> Vec<NodeHandle> {
        // ZIM hyperlinks are the one neutral edge kind we expose.
        if !kinds.contains(&EdgeKind::Link) {
            return Vec::new();
        }
        // Lazy: parse ONLY this one article. No whole-archive walk.
        let Some(html) = self.content(h) else {
            return Vec::new();
        };
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for href in scan_hrefs(&html) {
            if let Some(url) = resolve_href(&href) {
                if url != h.id && seen.insert(url.clone()) {
                    out.push(self.handle(&url));
                }
            }
        }
        out
    }

    fn capabilities(&self) -> SourceCaps {
        SourceCaps {
            resolution: true,
            stable_identity: true,
            drift: true,
            blast_radius: true,
            implementations: false,
        }
    }
}

/// Scan `html` for `<a … href="…">` targets, returning each raw href string in document order.
///
/// A deliberately tiny, dependency-free scanner (the adapter must not pull an HTML parser): it walks
/// for `href=` inside anchor-ish tags and reads the quoted value. Adapted from the link-extraction
/// shape used by `zxr`'s viewer / `app/src/panels/zim.rs` (which hit-tests laid-out links rather
/// than scanning source); here we have only the bytes, so we scan the source directly.
fn scan_hrefs(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find("href") {
        let mut i = search_from + rel + "href".len();
        search_from = i;
        // Skip whitespace then require '='.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // Quoted ("…" or '…') or bare (up to whitespace / '>').
        let (value, end) = match bytes[i] {
            q @ (b'"' | b'\'') => {
                let start = i + 1;
                match html[start..].find(q as char) {
                    Some(off) => (&html[start..start + off], start + off + 1),
                    None => break,
                }
            }
            _ => {
                let start = i;
                let off = html[start..]
                    .find(|c: char| c.is_whitespace() || c == '>')
                    .unwrap_or(html.len() - start);
                (&html[start..start + off], start + off)
            }
        };
        if !value.is_empty() {
            out.push(value.to_string());
        }
        search_from = end.max(search_from + 1);
    }
    out
}

/// Turn an in-archive `href` into a content-article URL, or `None` if it is not an in-archive
/// navigation (external scheme, or a pure `#fragment`).
///
/// Carried (with edits) from `app/src/panels/zim.rs::resolve_href`. The original used
/// `hiker_core::url::classify` for external detection; this adapter is hiker-core-free, so we use a
/// plain scheme check instead: bail on any `scheme://` URI, `mailto:`, or a leading `#`. The rest
/// (drop `#frag`/`?query`, strip leading `./`/`../`, drop a single-letter namespace dir, percent-
/// decode) is unchanged so URLs match the archive's stored entry path.
fn resolve_href(href: &str) -> Option<String> {
    // External / non-archive schemes leave the archive.
    if href.starts_with('#') || href.starts_with("mailto:") || href.contains("://") {
        return None;
    }
    // Pure fragment / empty path: nothing to navigate to.
    let no_frag = href.split('#').next().unwrap_or("");
    let path = no_frag.split('?').next().unwrap_or("");
    if path.is_empty() {
        return None;
    }

    // Normalize leading relative segments.
    let mut s = path;
    loop {
        if let Some(rest) = s.strip_prefix("./") {
            s = rest;
        } else if let Some(rest) = s.strip_prefix("../") {
            s = rest;
        } else {
            break;
        }
    }
    // Drop a leading single-letter namespace dir (`A/Foo`, `C/Foo`).
    if let Some(rest) = s.strip_prefix("A/").or_else(|| s.strip_prefix("C/")) {
        s = rest;
    }

    Some(percent_decode(s))
}

/// Minimal percent-decoding (`%XX` → byte) for article URLs. Carried verbatim from
/// `app/src/panels/zim.rs::percent_decode` — pure-Rust, no extra dep. ZIM hrefs use UTF-8 percent
/// escapes for non-ASCII titles.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // --- Synthetic in-memory ZIM builder -------------------------------------------------------
    //
    // Hand-assembles a minimal but spec-valid uncompressed ZIM (header-resident title pointer
    // list), mirroring the builder in `zxr/src/zim.rs` tests + `app/src/panels/zim.rs::tiny_zim`.
    // Each content entry gets its own single-blob uncompressed cluster. NO real `.zim` file —
    // hermetic. Written to a temp file only because `Zim::open` takes a path (mmap).

    const ZIM_MAGIC: u32 = 0x044D_495A;

    struct Article {
        url: &'static str,
        title: &'static str,
        body: &'static [u8],
    }

    fn push_zstring(buf: &mut Vec<u8>, s: &str) {
        buf.extend_from_slice(s.as_bytes());
        buf.push(0);
    }

    fn build_cluster(data: &[u8]) -> Vec<u8> {
        let mut c = vec![0u8]; // info byte: comp=0 (uncompressed).
        let first = 8u32; // 2 offsets * 4 bytes.
        c.extend_from_slice(&first.to_le_bytes());
        c.extend_from_slice(&(first + data.len() as u32).to_le_bytes());
        c.extend_from_slice(data);
        c
    }

    /// Assemble a complete in-memory ZIM from content-namespace `articles` (all `C`, mime 0 =
    /// text/html). Title pointer list is built sorted by title as the spec requires.
    fn build_archive(articles: &[Article], main_page: usize) -> Vec<u8> {
        let entry_count = articles.len() as u32;
        let cluster_count = articles.len() as u32;

        let mut mime_blob = Vec::new();
        push_zstring(&mut mime_blob, "text/html");
        mime_blob.push(0); // empty terminator.

        // Dir-entry index space, sorted by (namespace, url) — here all `C`, so by url.
        let mut order: Vec<usize> = (0..articles.len()).collect();
        order.sort_by(|&a, &b| articles[a].url.cmp(articles[b].url));
        let mut dir_index = vec![0u32; articles.len()];
        for (i, &a) in order.iter().enumerate() {
            dir_index[a] = i as u32;
        }

        let mut entry_bodies: Vec<Vec<u8>> = Vec::new();
        for &a in &order {
            let art = &articles[a];
            let mut b = Vec::new();
            b.extend_from_slice(&0u16.to_le_bytes()); // mime id 0
            b.push(0); // parameter len
            b.push(b'C'); // namespace
            b.extend_from_slice(&0u32.to_le_bytes()); // revision
            b.extend_from_slice(&dir_index[a].to_le_bytes()); // cluster == its dir slot
            b.extend_from_slice(&0u32.to_le_bytes()); // blob 0
            push_zstring(&mut b, art.url);
            push_zstring(&mut b, art.title);
            entry_bodies.push(b);
        }

        // Clusters in dir-entry order (cluster i == dir slot i).
        let clusters: Vec<Vec<u8>> =
            order.iter().map(|&a| build_cluster(articles[a].body)).collect();

        let header_len = 80u64;
        let mime_pos = header_len;
        let url_ptr_pos = mime_pos + mime_blob.len() as u64;
        let title_ptr_pos = url_ptr_pos + entry_count as u64 * 8;
        let cluster_ptr_pos = title_ptr_pos + entry_count as u64 * 4;
        let entries_pos = cluster_ptr_pos + cluster_count as u64 * 8;

        let mut entry_offsets = Vec::new();
        let mut cur = entries_pos;
        for b in &entry_bodies {
            entry_offsets.push(cur);
            cur += b.len() as u64;
        }
        let mut cluster_offsets = Vec::new();
        for c in &clusters {
            cluster_offsets.push(cur);
            cur += c.len() as u64;
        }
        let checksum_pos = cur;

        // Title pointer list: dir indices ordered by title.
        let mut by_title: Vec<(&str, u32)> =
            articles.iter().enumerate().map(|(i, a)| (a.title, dir_index[i])).collect();
        by_title.sort_by(|a, b| a.0.cmp(b.0));

        let mut out = vec![0u8; 80];
        out[0..4].copy_from_slice(&ZIM_MAGIC.to_le_bytes());
        out[24..28].copy_from_slice(&entry_count.to_le_bytes());
        out[28..32].copy_from_slice(&cluster_count.to_le_bytes());
        out[32..40].copy_from_slice(&url_ptr_pos.to_le_bytes());
        out[40..48].copy_from_slice(&title_ptr_pos.to_le_bytes());
        out[48..56].copy_from_slice(&cluster_ptr_pos.to_le_bytes());
        out[56..64].copy_from_slice(&mime_pos.to_le_bytes());
        out[64..68].copy_from_slice(&dir_index[main_page].to_le_bytes());
        out[68..72].copy_from_slice(&0xffff_ffffu32.to_le_bytes()); // layout page
        out[72..80].copy_from_slice(&checksum_pos.to_le_bytes());

        out.extend_from_slice(&mime_blob);
        for off in &entry_offsets {
            out.extend_from_slice(&off.to_le_bytes());
        }
        for (_, idx) in &by_title {
            out.extend_from_slice(&idx.to_le_bytes());
        }
        for off in &cluster_offsets {
            out.extend_from_slice(&off.to_le_bytes());
        }
        for b in &entry_bodies {
            out.extend_from_slice(b);
        }
        for c in &clusters {
            out.extend_from_slice(c);
        }
        out.extend_from_slice(&[0u8; 16]); // checksum
        out
    }

    /// A 3-article ZIM: Apple links to Banana, Cherry, an external https URL, and a `#frag`.
    fn sample() -> (ZimAdapter, tempfile::NamedTempFile) {
        let apple_body = br##"<html><body>
            <p>An <a href="Banana">banana</a> and a <a href="./Cherry">cherry</a>.</p>
            <p>See <a href="https://example.com/x">the web</a> and a <a href="#section">section</a>.</p>
            <p>Repeat <a href="C/Banana">banana again</a>.</p>
        </body></html>"##;
        let articles = [
            Article { url: "Apple", title: "Apple", body: apple_body },
            Article { url: "Banana", title: "Banana", body: b"<html>banana</html>" },
            Article { url: "Cherry", title: "Cherry", body: b"<html>cherry</html>" },
        ];
        let bytes = build_archive(&articles, 0);
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&bytes).unwrap();
        f.flush().unwrap();
        let adapter = ZimAdapter::open(f.path(), SourceId("wiki".into())).unwrap();
        (adapter, f)
    }

    #[test]
    fn resolve_finds_article_by_title() {
        let (a, _f) = sample();
        let h = a.resolve("Apple", &SourceId("wiki".into())).expect("resolve Apple");
        assert_eq!(h.id, "Apple");
        assert_eq!(h.source, SourceId("wiki".into()));
        // Wrong scope → None.
        assert!(a.resolve("Apple", &SourceId("other".into())).is_none());
        // No prefix match → None.
        assert!(a.resolve("Zzz", &SourceId("wiki".into())).is_none());
    }

    #[test]
    fn locate_and_content_for_article() {
        let (a, _f) = sample();
        let h = NodeHandle { source: SourceId("wiki".into()), id: "Banana".into() };
        let loc = a.locate(&h).unwrap();
        assert_eq!(loc, SourceLoc { file: "C/Banana".into(), start_line: 0, end_line: 0 });
        assert_eq!(a.content(&h).unwrap(), "<html>banana</html>");
    }

    #[test]
    fn neighbors_parses_one_article_drops_external_and_frag() {
        let (a, _f) = sample();
        let apple = NodeHandle { source: SourceId("wiki".into()), id: "Apple".into() };
        let mut ids: Vec<String> =
            a.neighbors(&apple, &[EdgeKind::Link]).into_iter().map(|h| h.id).collect();
        ids.sort();
        // Banana + Cherry only: external https + `#section` dropped, duplicate Banana deduped.
        assert_eq!(ids, vec!["Banana".to_string(), "Cherry".to_string()]);

        // Without the Link kind requested, no neighbors (and no parse).
        assert!(a.neighbors(&apple, &[EdgeKind::Calls]).is_empty());
    }

    #[test]
    fn fingerprint_is_stable() {
        let (a, _f) = sample();
        let h = NodeHandle { source: SourceId("wiki".into()), id: "Banana".into() };
        let fp1 = a.fingerprint(&h).expect("fingerprint");
        let fp2 = a.fingerprint(&h).expect("fingerprint");
        assert_eq!(fp1, fp2, "fingerprint must be stable across calls");
        // A different article hashes differently.
        let cherry = NodeHandle { source: SourceId("wiki".into()), id: "Cherry".into() };
        assert_ne!(fp1, a.fingerprint(&cherry).unwrap());
    }

    #[test]
    fn capabilities_match_decision() {
        let (a, _f) = sample();
        let caps = a.capabilities();
        assert!(caps.resolution && caps.stable_identity && caps.drift && caps.blast_radius);
        assert!(!caps.implementations);
    }

    #[test]
    fn scan_and_resolve_href_units() {
        let hrefs = scan_hrefs(r#"<a href="Foo">x</a> <a href='./Bar#s'>y</a> <a href=Baz>z</a>"#);
        assert_eq!(hrefs, vec!["Foo", "./Bar#s", "Baz"]);
        assert_eq!(resolve_href("Article_Name"), Some("Article_Name".into()));
        assert_eq!(resolve_href("../A/Foo"), Some("Foo".into()));
        assert_eq!(resolve_href("C/Bar"), Some("Bar".into()));
        assert_eq!(resolve_href("Foo#section"), Some("Foo".into()));
        assert_eq!(resolve_href("#only"), None);
        assert_eq!(resolve_href("https://example.com"), None);
        assert_eq!(resolve_href("mailto:a@b.c"), None);
        assert_eq!(percent_decode("Caf%C3%A9"), "Café");
    }
}
