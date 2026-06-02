//! LLM-assisted source-plugin authoring (`crawler-emit-mode`).
//!
//! The deterministic emitter templates the picked selectors straight into a Lua
//! skeleton; this module is the opt-in alternative that hands the selection to a
//! model and asks it to author a robust per-site extractor. It reuses hiker's
//! ONE generative client (`hiker_llm`, `crawler-shared-llm`) so there is no
//! second provider layer, and deliberately does NOT pull in `hiker-core`: the
//! [`hiker_llm::ProviderConfig`] is assembled from environment variables here.
//!
//! eframe's update loop is synchronous, so the async `Client::chat` is driven by
//! a current-thread `tokio` runtime `block_on`. That blocks the UI for the
//! duration of the call — acceptable for this opt-in v1 (noted below); a later
//! pass can background it.

use hiker_llm::{Client, GraniteLlmClient, Message, ProviderConfig};

use crate::picker::Selection;

/// Environment knobs for the crawler's LLM backend. All optional; sensible
/// Anthropic defaults keep the common case zero-config beyond the API key.
const ENV_BACKEND: &str = "HIKER_CRAWLER_LLM_BACKEND";
const ENV_MODEL: &str = "HIKER_CRAWLER_LLM_MODEL";
const ENV_API_KEY_ENV: &str = "HIKER_CRAWLER_LLM_API_KEY_ENV";
const ENV_BASE_URL: &str = "HIKER_CRAWLER_LLM_BASE_URL";

const DEFAULT_BACKEND: &str = "anthropic";
const DEFAULT_MODEL: &str = "claude-sonnet-4-5";
const DEFAULT_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// Author a per-site Lua extractor for `sel` via the shared LLM client
/// (`crawler-emit-mode`). Returns the model's Lua on success, or a clear,
/// user-facing message (never a panic) when the backend is unconfigured or the
/// call fails — including a "set `ANTHROPIC_API_KEY`…" hint when the key env is
/// missing.
///
/// Blocks the calling (UI) thread for the round-trip; this is the accepted v1
/// trade-off.
// TODO(crawler-emit-mode): background the call so the UI stays responsive while
// the model authors the extractor.
// status: crawler-emit-mode
#[must_use]
pub fn author_source_plugin(sel: &Selection) -> String {
    let cfg = provider_config();
    // Surface a missing key before building the client, with the actual env var
    // name so the user knows exactly what to set.
    let key_env = cfg.api_key_env.clone().unwrap_or_default();
    if std::env::var(&key_env).is_err() {
        return format!(
            "-- LLM authoring needs an API key.\n\
             -- Set {key_env} in the environment (or point {ENV_API_KEY_ENV} at a different \
             var, and set {ENV_BACKEND}/{ENV_MODEL} for a non-Anthropic backend), then retry.\n"
        );
    }

    let client = match GraniteLlmClient::new(cfg) {
        Ok(c) => c,
        Err(e) => return format!("-- LLM client init failed: {e}\n"),
    };

    let messages = build_messages(sel);
    match block_on_chat(&client, &messages) {
        Ok(lua) => lua,
        Err(e) => format!("-- LLM authoring failed: {e}\n"),
    }
}

/// Build the [`ProviderConfig`] from the environment (`crawler-emit-mode`),
/// falling back to Anthropic defaults. The API key itself is never read here —
/// `api_key_env` names the var and `hiker_llm` reads it at call time.
fn provider_config() -> ProviderConfig {
    let env = |name: &str| std::env::var(name).ok().filter(|s| !s.is_empty());
    ProviderConfig {
        backend: env(ENV_BACKEND).unwrap_or_else(|| DEFAULT_BACKEND.to_owned()),
        model: env(ENV_MODEL).unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
        api_key: None,
        api_key_env: Some(env(ENV_API_KEY_ENV).unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_owned())),
        base_url: env(ENV_BASE_URL),
        max_tokens: Some(4096),
        timeout_secs: Some(120),
    }
}

/// The system + user prompt pair handed to the model: a system role pinning the
/// output contract, and a user role carrying the seed URL and the picked
/// `{field, selector, sample}` triples.
fn build_messages(sel: &Selection) -> Vec<Message> {
    let mut picks = String::new();
    for f in &sel.fields {
        let sample = f.sample.chars().take(160).collect::<String>();
        picks.push_str(&format!(
            "- field `{}` | selector `{}` | repeat: {} | sample: {}\n",
            f.name, f.selector, f.repeat, sample.replace('\n', " ")
        ));
    }
    if picks.is_empty() {
        picks.push_str("(no fields picked — infer the main content from the page)\n");
    }

    let system = "You author per-site web extractors for the hiker note app as Lua scripts. \
        The extractor's `extract(doc, url)` entry point must return a table \
        `{ markdown = <string>, frontmatter = <table?>, next_urls = <list of strings?> }`. \
        Use the provided CSS selectors as a starting point but make the script robust to \
        minor markup changes (fallbacks, trimming). When a field is marked repeat:true the \
        page is a listing/hub — collect its matches' hrefs into `next_urls`. Prefer fetching \
        a site's JSON/API endpoint when the page clearly hydrates from one (an API-fetch \
        pattern), falling back to DOM scraping otherwise. Output ONLY the Lua source, no \
        prose and no markdown code fences.";

    let user = format!(
        "Seed URL: {seed}\n\nPicked fields:\n{picks}\nWrite the Lua extractor.",
        seed = sel.seed_url,
    );

    vec![Message::system(system), Message::user(user)]
}

/// Drive the async `chat` on a fresh current-thread `tokio` runtime. eframe is
/// synchronous, so a dedicated `block_on` is the simplest correct bridge for
/// this opt-in, UI-blocking v1.
fn block_on_chat(client: &GraniteLlmClient, messages: &[Message]) -> Result<String, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    runtime
        .block_on(client.chat(messages))
        .map_err(|e| e.to_string())
}
