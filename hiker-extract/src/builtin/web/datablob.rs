//! The server-rendered data-blob probe. Before falling back on a thin
//! readability result, the web extractor checks for structured content already
//! embedded in the static HTML — `<script id="__NEXT_DATA__">` (Next.js),
//! `window.__NUXT__` (Nuxt), and `<script type="application/ld+json">`
//! (schema.org JSON-LD). This JSON is **parsed, never executed**, and pulls a
//! large slice of framework-rendered "SPA" sites into reach without running any
//! of their JavaScript. See `docs/extract.md` `extract-web-data-blob`.
//
// status: extract-web-data-blob

use scraper::{Html, Selector};
use serde_json::Value;

/// Content recovered from a server-rendered data blob: a title (if present)
/// and a markdown-ish body assembled from the JSON's article fields. Returned
/// only when the probe found a usable `articleBody` / `headline` / text — an
/// empty find yields `None` so the caller continues down the fallback chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Blob {
    pub title: Option<String>,
    pub body: String,
}

/// Probe `html` for a usable server-rendered data blob, in priority order:
/// JSON-LD `Article` (the most structured + standardized), then `__NEXT_DATA__`,
/// then `window.__NUXT__`. The first that yields a non-empty body wins. JSON is
/// parsed with `serde_json` — nothing is executed.
pub(super) fn probe(html: &str) -> Option<Blob> {
    let doc = Html::parse_document(html);
    json_ld_article(&doc)
        .or_else(|| script_json_blob(&doc, "__NEXT_DATA__"))
        .or_else(|| nuxt_blob(html))
}

/// Pull an `Article`/`NewsArticle`/`BlogPosting` out of any
/// `<script type="application/ld+json">` block. schema.org articles carry
/// `headline` + `articleBody`, which is exactly a title + body.
fn json_ld_article(doc: &Html) -> Option<Blob> {
    let sel = Selector::parse(r#"script[type="application/ld+json"]"#).ok()?;
    for el in doc.select(&sel) {
        let text = el.text().collect::<String>();
        let Ok(json) = serde_json::from_str::<Value>(&text) else { continue };
        // JSON-LD may be a single object, an array, or a @graph wrapper.
        for node in flatten_ld(&json) {
            if let Some(blob) = ld_article_node(node) {
                return Some(blob);
            }
        }
    }
    None
}

/// Yield the candidate article nodes from a parsed JSON-LD value: the value
/// itself, every element of a top-level array, and every element of a
/// `@graph`.
fn flatten_ld(json: &Value) -> Vec<&Value> {
    let mut out = vec![json];
    if let Some(arr) = json.as_array() {
        out.extend(arr.iter());
    }
    if let Some(graph) = json.get("@graph").and_then(Value::as_array) {
        out.extend(graph.iter());
    }
    out
}

/// Turn one JSON-LD node into a [`Blob`] if it is an article type with a body.
fn ld_article_node(node: &Value) -> Option<Blob> {
    let ty = node.get("@type")?;
    let is_article = ld_type_matches(ty);
    if !is_article {
        return None;
    }
    let body = node.get("articleBody").and_then(Value::as_str)?.trim().to_string();
    if body.is_empty() {
        return None;
    }
    let title = node
        .get("headline")
        .or_else(|| node.get("name"))
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Some(Blob { title, body })
}

/// Whether a JSON-LD `@type` (a string or an array of strings) names an
/// article-like schema.org type.
fn ld_type_matches(ty: &Value) -> bool {
    const ARTICLE_TYPES: &[&str] = &["Article", "NewsArticle", "BlogPosting", "Report"];
    match ty {
        Value::String(s) => ARTICLE_TYPES.contains(&s.as_str()),
        Value::Array(arr) => arr
            .iter()
            .filter_map(Value::as_str)
            .any(|s| ARTICLE_TYPES.contains(&s)),
        _ => false,
    }
}

/// Pull the JSON from a `<script id="...">JSON</script>` blob (the
/// `__NEXT_DATA__` shape) and harvest article-ish text out of it. Next.js
/// nests page props arbitrarily, so we walk the whole tree for the longest
/// string under a likely key.
fn script_json_blob(doc: &Html, id: &str) -> Option<Blob> {
    let sel = Selector::parse(&format!(r#"script[id="{id}"]"#)).ok()?;
    let el = doc.select(&sel).next()?;
    let text = el.text().collect::<String>();
    let json = serde_json::from_str::<Value>(&text).ok()?;
    harvest_props(&json)
}

/// Parse the `window.__NUXT__ = {...}` assignment Nuxt emits inline and harvest
/// article text from it. We slice the object literal out of the script source
/// (the assignment's right-hand side) and parse it as JSON; Nuxt's payload is
/// JSON-compatible for the data we want even though the wrapper is JS.
fn nuxt_blob(html: &str) -> Option<Blob> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse("script").ok()?;
    for el in doc.select(&sel) {
        let src = el.text().collect::<String>();
        let Some(idx) = src.find("__NUXT__") else { continue };
        let after = &src[idx..];
        let Some(eq) = after.find('=') else { continue };
        let rhs = after[eq + 1..].trim();
        let Some(obj) = balanced_object(rhs) else { continue };
        if let Ok(json) = serde_json::from_str::<Value>(obj)
            && let Some(blob) = harvest_props(&json)
        {
            return Some(blob);
        }
    }
    None
}

/// Slice the first balanced `{...}` object literal off the front of `s`
/// (string-aware, so braces inside quotes don't miscount). Returns the slice
/// including both braces, or `None` if unbalanced.
fn balanced_object(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'{') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Walk an arbitrary JSON tree and recover an article title + body: the value
/// under an `articleBody`/`content`/`body` key as the body, and the value under
/// a `title`/`headline` key as the title. Picks the longest body string found,
/// so the real article content beats incidental short fields.
fn harvest_props(json: &Value) -> Option<Blob> {
    let mut best_body: Option<String> = None;
    let mut title: Option<String> = None;
    walk(json, &mut best_body, &mut title);
    let body = best_body?;
    if body.chars().filter(|c| !c.is_whitespace()).count() < MIN_BLOB_BODY {
        return None;
    }
    Some(Blob { title, body })
}

/// Minimum non-whitespace body length for a harvested data-blob body to count
/// as real content. Below this we'd rather let the readability/fallback path
/// try than commit a stub.
const MIN_BLOB_BODY: usize = 200;

/// Recursive harvest helper: descend maps/arrays, tracking the longest
/// body-keyed string and the first title-keyed string.
fn walk(node: &Value, best_body: &mut Option<String>, title: &mut Option<String>) {
    const BODY_KEYS: &[&str] = &["articleBody", "content", "body", "bodyHtml", "html"];
    const TITLE_KEYS: &[&str] = &["title", "headline", "name"];
    match node {
        Value::Object(map) => {
            for (k, v) in map {
                if let Value::String(s) = v {
                    if BODY_KEYS.contains(&k.as_str()) {
                        let longer = best_body.as_ref().is_none_or(|b| b.len() < s.len());
                        if longer {
                            *best_body = Some(s.clone());
                        }
                    } else if TITLE_KEYS.contains(&k.as_str()) && title.is_none() && !s.trim().is_empty() {
                        *title = Some(s.trim().to_string());
                    }
                }
                walk(v, best_body, title);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                walk(v, best_body, title);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::probe;

    #[test]
    fn parses_json_ld_article() {
        let html = r#"<html><head>
            <script type="application/ld+json">
            {"@context":"https://schema.org","@type":"NewsArticle",
             "headline":"Breaking: JSON-LD Works",
             "articleBody":"This is the full article body recovered straight from the JSON-LD blob without running any JavaScript at all."}
            </script></head><body><div id="root"></div></body></html>"#;
        let blob = probe(html).expect("json-ld article found");
        assert_eq!(blob.title.as_deref(), Some("Breaking: JSON-LD Works"));
        assert!(blob.body.contains("recovered straight from the JSON-LD blob"));
    }

    #[test]
    fn parses_json_ld_graph() {
        let html = r#"<html><head>
            <script type="application/ld+json">
            {"@graph":[{"@type":"WebPage"},
              {"@type":"BlogPosting","headline":"Graphed",
               "articleBody":"Body text living inside a @graph wrapper, long enough to clear the minimum body length gate that the harvester enforces on blobs."}]}
            </script></head><body></body></html>"#;
        let blob = probe(html).expect("graph article found");
        assert_eq!(blob.title.as_deref(), Some("Graphed"));
        assert!(blob.body.contains("inside a @graph wrapper"));
    }

    #[test]
    fn parses_next_data_blob() {
        let html = r#"<html><body><div id="__next"></div>
            <script id="__NEXT_DATA__" type="application/json">
            {"props":{"pageProps":{"post":{"title":"Next Post",
              "content":"The article content nested deep inside Next.js page props, recovered by walking the parsed JSON tree for the longest body-keyed string here. This sentence pads the body well past the minimum length gate so the harvester accepts it as a genuine article rather than an incidental short field."}}}}
            </script></body></html>"#;
        let blob = probe(html).expect("next data found");
        assert_eq!(blob.title.as_deref(), Some("Next Post"));
        assert!(blob.body.contains("nested deep inside Next.js page props"));
    }

    #[test]
    fn parses_nuxt_blob() {
        let html = r#"<html><body>
            <script>window.__NUXT__={"data":[{"article":{"headline":"Nuxt One",
              "body":"Nuxt server-rendered article body sliced out of the inline assignment and parsed as JSON, long enough to pass the body-length gate. A second clause keeps the harvested body comfortably above the minimum so the probe treats it as a real article."}}]}</script>
            </body></html>"#;
        let blob = probe(html).expect("nuxt blob found");
        assert_eq!(blob.title.as_deref(), Some("Nuxt One"));
        assert!(blob.body.contains("server-rendered article body"));
    }

    #[test]
    fn no_blob_yields_none() {
        let html = "<html><body><p>just a plain page, no framework blob</p></body></html>";
        assert!(probe(html).is_none());
    }

    #[test]
    fn short_blob_body_rejected() {
        // A blob whose body is too short to be a real article is declined so the
        // readability/fallback path can try.
        let html = r#"<html><body><script id="__NEXT_DATA__">
            {"props":{"content":"too short"}}</script></body></html>"#;
        assert!(probe(html).is_none());
    }
}
