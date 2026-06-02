//! Config bridge from core's `[llm]` TOML section to the shared LLM client.
//!
//! The generative-LLM client itself now lives in the `hiker-llm` leaf crate
//! (so `hiker-core` and `hiker-crawler` share ONE client). What stays here is
//! only the translation that references core's own config types
//! (`crate::config::sections::*`) — it can't live in `hiker-llm` without that
//! crate depending on core. This module imports `hiker_llm` types only; the
//! `llm` crate is imported in `hiker-llm` and nowhere else.
//
// status: llm-core-module

use crate::config::sections::{LlmConfig, LlmLimitsConfig, LlmProviderConfig};

/// Build a `hiker_llm::GraniteLlmClient` from a loaded `[llm]` section. Empty
/// strings in `api_key_env` / `base_url` map to `None` (the TOML auto-create
/// writes them as empty strings). Replaces the old
/// `GraniteLlmClient::from_config` inherent method, which couldn't move to
/// `hiker-llm` because it reads core config types.
pub fn client_from_config(
    cfg: &LlmConfig,
) -> Result<hiker_llm::GraniteLlmClient, hiker_llm::Error> {
    hiker_llm::GraniteLlmClient::new(provider_config_from(&cfg.provider, &cfg.limits))
}

/// Build a connection-shaped `ProviderConfig` from the loaded TOML section.
/// Empty `api_key_env` / `base_url` strings map to `None` so the builder
/// only sets the corresponding field when the user actually configured it.
pub fn provider_config_from(
    p: &LlmProviderConfig,
    l: &LlmLimitsConfig,
) -> hiker_llm::ProviderConfig {
    hiker_llm::ProviderConfig {
        backend: p.backend.clone(),
        model: p.model.clone(),
        api_key: empty_to_none(&p.api_key),
        api_key_env: empty_to_none(&p.api_key_env),
        base_url: empty_to_none(&p.base_url),
        max_tokens: Some(l.max_tokens),
        timeout_secs: Some(l.timeout_secs),
    }
}

fn empty_to_none(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}
