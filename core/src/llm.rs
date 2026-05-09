//! Generative LLM access. See `docs/llm.md`.
//!
//! Wraps the [`llm`](https://crates.io/crates/llm) crate (graniet/llm) so
//! every backend (Anthropic, OpenAI, Ollama, Google, Groq, Mistral, DeepSeek,
//! OpenRouter, ...) reaches callers through one narrow trait. Module
//! discipline mirrors `core::store` (rusqlite-only) and `core::embed`
//! (fastembed-only): the `llm` crate is imported here and nowhere else, so
//! swapping the multi-provider layer or adding a fallback is a one-file
//! change.
//!
//! Two consumers in v3.5: background / fan-out features call `chat`
//! single-shot; the basic agent loop (`core::agent`) calls `chat_stream` for
//! interactive surfaces. Tool-calling, audit logging, and per-call cost
//! transparency live in those consumers — `core::llm` is just provider
//! access. Embeddings are a separate concern (`core::embed`); they share the
//! same crate dep but a different trait boundary.
//
// status: llm-core-module

use std::pin::Pin;

use async_trait::async_trait;
use futures::stream::{Stream, StreamExt};
use llm::builder::{FunctionBuilder, LLMBackend, LLMBuilder};
use llm::chat::ChatMessage;
use llm::error::LLMError;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::{LlmConfig, LlmLimitsConfig, LlmProviderConfig};

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("unknown backend: {0}")]
    UnknownBackend(String),
    #[error("missing api key: env var {0} is unset")]
    MissingApiKey(String),
    #[error("provider build: {0}")]
    Build(String),
    #[error("provider call: {0}")]
    Provider(String),
    #[error("empty response from provider")]
    EmptyResponse,
    /// The client doesn't implement the requested method (typically the
    /// streaming-with-tools variant on a mock or simple client). Callers
    /// match on this variant to fall back rather than scraping a prose
    /// message — the default trait impl returns this with a stable
    /// `feature` discriminator.
    #[error("unsupported feature on this client: {feature}")]
    Unsupported { feature: &'static str },
}

impl From<LLMError> for LlmError {
    fn from(e: LLMError) -> Self {
        LlmError::Provider(e.to_string())
    }
}

/// Role of a participant in the message history. The crate's `ChatRole` is
/// User/Assistant only; `System` is set as a separate builder field at
/// provider construction time. Our trait lets callers express system
/// content as just another `Message`, and the impl does the demultiplexing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Tool calls the assistant requested in this message. Populated only
    /// for `Role::Assistant` messages produced by `chat_with_tools`; the
    /// agent loop carries them back into history so the next provider call
    /// sees a coherent tool-use → tool-result sequence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Tool results the user side is feeding back. Populated only for
    /// `Role::User` messages emitted by the agent loop after dispatching a
    /// tool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_results: Vec<ToolResult>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        }
    }
}

/// One tool-call request emitted by the assistant. Mirrors the `llm`
/// crate's `ToolCall` shape but lives in our re-exported namespace so the
/// agent module never imports `llm::*`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON-encoded arguments. The provider hands these back as a string,
    /// so we keep them that way until the dispatcher parses.
    pub arguments: String,
}

/// One tool-result feed-back row. `output` is whatever the dispatcher
/// produced (usually JSON-encoded so the model can parse it); `ok = false`
/// signals failure / timeout, in which case `output` is a short
/// human-readable reason.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub output: String,
    pub ok: bool,
}

/// One JSON-Schema-shaped tool definition. The `parameters` value is a
/// raw JSON Schema object — callers compose it with `serde_json::json!`
/// or hand-build it; the agent loop passes it straight through to the
/// provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Result of a non-streaming tool-aware completion. Either the model
/// produced terminal text (`text` set, `tool_calls` empty) or it asked to
/// run one or more tools (`tool_calls` non-empty; `text` may still carry
/// preamble text the model emitted alongside the call).
#[derive(Debug, Clone, Default)]
pub struct ToolChatResponse {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

/// One chunk in a streaming-with-tools response. The model interleaves
/// text deltas with tool-call lifecycle events; this enum normalizes the
/// `llm` crate's `StreamChunk` shape so the agent module never imports
/// `llm::*`. `index` orders concurrent tool calls within one model
/// response (the same `index` participates in `ToolUseStart`,
/// `ToolUseInputDelta`, `ToolUseComplete` for one call).
#[derive(Debug, Clone)]
pub enum AgentChunk {
    Text(String),
    ToolUseStart { index: usize, call_id: String, name: String },
    ToolUseInputDelta { index: usize, partial_args: String },
    ToolUseComplete { index: usize, call: ToolCall },
    Done { stop_reason: String },
}

pub type ToolStream =
    Pin<Box<dyn Stream<Item = Result<AgentChunk, LlmError>> + Send>>;

/// Connection-shaped config for a provider. Mirrors the `[provider]` table
/// in `vault/.hiker/llm.toml` (see `llm-providers-config`); this struct is
/// the in-memory shape the loader will populate. API keys are never stored
/// here directly — `api_key_env` names an environment variable and the
/// builder reads it at construction time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub backend: String,
    pub model: String,
    /// Literal API key. When `Some`, takes precedence over `api_key_env`.
    /// Sourced from the user-scope TOML's `[llm.provider].api_key` field
    /// per `llm.md` §`[llm-providers-config]`; the vault TOML cannot
    /// carry this value (eligibility list refuses it).
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Stream item type for `chat_stream`: text deltas as they arrive from the
/// provider. Errors mid-stream surface as `Err`; the consumer decides
/// whether to abort the turn.
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<String, LlmError>> + Send>>;

/// Narrow, opaque generative-LLM interface. The concrete implementation
/// (graniet/`llm` today, possibly a different multi-provider crate or a
/// custom HTTP client later) lives behind this trait so swapping it is a
/// one-module change. Background and fan-out callers use `chat`; the basic
/// agent loop uses `chat_stream` so the chat panel can render tokens as
/// they arrive.
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError>;

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError>;

    /// Tool-aware single-shot completion used by the basic agent loop.
    /// Default impl errors so non-tool clients (e.g. the mock) don't have
    /// to override.
    async fn chat_with_tools(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<ToolChatResponse, LlmError> {
        Err(LlmError::Unsupported { feature: "chat_with_tools" })
    }

    /// Tool-aware streaming. Same input as `chat_with_tools`, but emits an
    /// `AgentChunk` per provider chunk so the agent loop can render text
    /// deltas + tool-call lifecycle events in source order.
    async fn chat_stream_with_tools(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<ToolStream, LlmError> {
        Err(LlmError::Unsupported { feature: "chat_stream_with_tools" })
    }
}

/// Production client backed by graniet/`llm`. Holds the resolved config and
/// rebuilds the underlying provider per call so per-request system prompts
/// (the leading `Role::System` message in the slice) flow through cleanly.
/// Build cost is small relative to the network round-trip.
pub struct GraniteLlmClient {
    cfg: ProviderConfig,
}

impl GraniteLlmClient {
    pub fn new(cfg: ProviderConfig) -> Result<Self, LlmError> {
        // Validate the backend name early so a misconfigured TOML fails at
        // client construction rather than on the first chat call.
        parse_backend(&cfg.backend)?;
        Ok(Self { cfg })
    }

    /// Build a `ProviderConfig` from a loaded `[llm]` section. Empty
    /// strings in `api_key_env` / `base_url` map to `None` (the TOML
    /// auto-create writes them as empty strings).
    pub fn from_config(cfg: &LlmConfig) -> Result<Self, LlmError> {
        Self::new(provider_config_from(&cfg.provider, &cfg.limits))
    }

    fn build_provider(
        &self,
        system_prompt: Option<&str>,
        tools: &[ToolDef],
    ) -> Result<Box<dyn llm::LLMProvider>, LlmError> {
        let backend = parse_backend(&self.cfg.backend)?;
        let mut b = LLMBuilder::new()
            .backend(backend)
            .model(&self.cfg.model);

        // Precedence (per `llm.md` §`[llm-providers-config]`):
        //   1. `api_key` literal (user-scope TOML only)
        //   2. `api_key_env` (env var named here, read at build time)
        // If neither is set we leave the builder without a key — matches
        // the local-Ollama case where no key is needed.
        if let Some(literal) = self.cfg.api_key.as_deref().filter(|s| !s.is_empty()) {
            b = b.api_key(literal.to_string());
        } else if let Some(env_var) = &self.cfg.api_key_env {
            let key = std::env::var(env_var)
                .map_err(|_| LlmError::MissingApiKey(env_var.clone()))?;
            b = b.api_key(key);
        }
        if let Some(url) = &self.cfg.base_url {
            b = b.base_url(url.clone());
        }
        if let Some(mt) = self.cfg.max_tokens {
            b = b.max_tokens(mt);
        }
        if let Some(t) = self.cfg.timeout_secs {
            b = b.timeout_seconds(t);
        }
        if let Some(sys) = system_prompt {
            b = b.system(sys.to_string());
        }
        for t in tools {
            b = b.function(
                FunctionBuilder::new(t.name.clone())
                    .description(t.description.clone())
                    .json_schema(t.parameters.clone()),
            );
        }

        b.build().map_err(|e| LlmError::Build(e.to_string()))
    }
}

/// Build a connection-shaped `ProviderConfig` from the loaded TOML section.
/// Empty `api_key_env` / `base_url` strings map to `None` so the builder
/// only sets the corresponding field when the user actually configured it.
pub fn provider_config_from(p: &LlmProviderConfig, l: &LlmLimitsConfig) -> ProviderConfig {
    ProviderConfig {
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

#[async_trait]
impl LlmClient for GraniteLlmClient {
    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
        let (system, rest) = split_system(messages);
        let provider = self.build_provider(system.as_deref(), &[])?;
        let chat_msgs = to_chat_messages(rest);
        let resp = provider.chat(&chat_msgs).await?;
        resp.text().ok_or(LlmError::EmptyResponse)
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        let (system, rest) = split_system(messages);
        let provider = self.build_provider(system.as_deref(), &[])?;
        let chat_msgs = to_chat_messages(rest);
        let inner = provider.chat_stream(&chat_msgs).await?;
        let mapped = inner.map(|item| item.map_err(LlmError::from));
        Ok(Box::pin(mapped))
    }

    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<ToolChatResponse, LlmError> {
        let (system, rest) = split_system(messages);
        let provider = self.build_provider(system.as_deref(), tools)?;
        let chat_msgs = to_chat_messages(rest);
        // Pass `None` as the tools slice — tools are already baked into the
        // builder via `.function(...)` above, mirroring the llm crate's
        // canonical usage. Passing `Some(&[])` would override that with an
        // empty list.
        let resp = provider.chat_with_tools(&chat_msgs, None).await?;
        let text = resp.text();
        let tool_calls = resp
            .tool_calls()
            .unwrap_or_default()
            .into_iter()
            .map(|t| ToolCall {
                id: t.id,
                name: t.function.name,
                arguments: t.function.arguments,
            })
            .collect();
        Ok(ToolChatResponse { text, tool_calls })
    }

    async fn chat_stream_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<ToolStream, LlmError> {
        use llm::chat::StreamChunk;
        let (system, rest) = split_system(messages);
        let provider = self.build_provider(system.as_deref(), tools)?;
        let chat_msgs = to_chat_messages(rest);
        let inner = provider.chat_stream_with_tools(&chat_msgs, None).await?;
        let mapped = inner.map(|item| match item {
            Ok(chunk) => Ok(match chunk {
                StreamChunk::Text(t) => AgentChunk::Text(t),
                StreamChunk::ToolUseStart { index, id, name } => {
                    AgentChunk::ToolUseStart { index, call_id: id, name }
                }
                StreamChunk::ToolUseInputDelta { index, partial_json } => {
                    AgentChunk::ToolUseInputDelta {
                        index,
                        partial_args: partial_json,
                    }
                }
                StreamChunk::ToolUseComplete { index, tool_call } => {
                    AgentChunk::ToolUseComplete {
                        index,
                        call: ToolCall {
                            id: tool_call.id,
                            name: tool_call.function.name,
                            arguments: tool_call.function.arguments,
                        },
                    }
                }
                StreamChunk::Done { stop_reason } => AgentChunk::Done { stop_reason },
            }),
            Err(e) => Err(LlmError::from(e)),
        });
        Ok(Box::pin(mapped))
    }
}

fn parse_backend(name: &str) -> Result<LLMBackend, LlmError> {
    name.parse::<LLMBackend>()
        .map_err(|_| LlmError::UnknownBackend(name.to_string()))
}

/// Split off any leading `Role::System` messages, concatenated newline-wise,
/// and return the rest. The `llm` crate accepts only one system prompt per
/// provider build, so multiple System messages collapse to one.
fn split_system(messages: &[Message]) -> (Option<String>, Vec<&Message>) {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut rest: Vec<&Message> = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role {
            Role::System => system_parts.push(&m.content),
            _ => rest.push(m),
        }
    }
    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };
    (system, rest)
}

fn to_chat_messages(messages: Vec<&Message>) -> Vec<ChatMessage> {
    use llm::{FunctionCall, ToolCall as LlmToolCall};
    let mut out: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role {
            Role::User if !m.tool_results.is_empty() => {
                // Tool results travel as a User message carrying
                // `MessageType::ToolResult`. The `arguments` slot holds the
                // result body (per the llm crate's convention — same shape
                // it uses for tool_use input).
                let tools: Vec<LlmToolCall> = m
                    .tool_results
                    .iter()
                    .map(|r| LlmToolCall {
                        id: r.call_id.clone(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: r.name.clone(),
                            arguments: r.output.clone(),
                        },
                    })
                    .collect();
                let mut msg = ChatMessage::user().tool_result(tools).build();
                if !m.content.is_empty() {
                    msg.content = m.content.clone();
                }
                out.push(msg);
            }
            Role::User => out.push(ChatMessage::user().content(&m.content).build()),
            Role::Assistant if !m.tool_calls.is_empty() => {
                let tools: Vec<LlmToolCall> = m
                    .tool_calls
                    .iter()
                    .map(|c| LlmToolCall {
                        id: c.id.clone(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: c.name.clone(),
                            arguments: c.arguments.clone(),
                        },
                    })
                    .collect();
                let mut msg = ChatMessage::assistant().tool_use(tools).build();
                if !m.content.is_empty() {
                    msg.content = m.content.clone();
                }
                out.push(msg);
            }
            Role::Assistant => out.push(ChatMessage::assistant().content(&m.content).build()),
            Role::System => unreachable!("split_system removed System messages"),
        }
    }
    out
}

/// Deterministic test double for callers that want to verify their own
/// behavior without network. Returns a canned response for `chat` and emits
/// the canned response in fixed-size chunks for `chat_stream`.
pub struct MockLlmClient {
    pub response: String,
}

impl MockLlmClient {
    pub fn new(response: impl Into<String>) -> Self {
        Self { response: response.into() }
    }
}

#[async_trait]
impl LlmClient for MockLlmClient {
    async fn chat(&self, _messages: &[Message]) -> Result<String, LlmError> {
        Ok(self.response.clone())
    }

    async fn chat_stream(&self, _messages: &[Message]) -> Result<ChatStream, LlmError> {
        let chunks: Vec<Result<String, LlmError>> = self
            .response
            .as_bytes()
            .chunks(8)
            .map(|c| Ok(String::from_utf8_lossy(c).into_owned()))
            .collect();
        Ok(Box::pin(futures::stream::iter(chunks)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_backend_known() {
        assert!(matches!(parse_backend("anthropic"), Ok(LLMBackend::Anthropic)));
        assert!(matches!(parse_backend("openai"), Ok(LLMBackend::OpenAI)));
        assert!(matches!(parse_backend("ollama"), Ok(LLMBackend::Ollama)));
    }

    #[test]
    fn parse_backend_unknown_errors() {
        match parse_backend("not-a-backend") {
            Err(LlmError::UnknownBackend(name)) => assert_eq!(name, "not-a-backend"),
            other => panic!("expected UnknownBackend, got {other:?}"),
        }
    }

    #[test]
    fn split_system_collects_leading_and_interspersed() {
        let msgs = vec![
            Message::system("rule 1"),
            Message::user("hi"),
            Message::system("rule 2"),
            Message::assistant("ack"),
        ];
        let (sys, rest) = split_system(&msgs);
        assert_eq!(sys.as_deref(), Some("rule 1\n\nrule 2"));
        assert_eq!(rest.len(), 2);
        assert_eq!(rest[0].role, Role::User);
        assert_eq!(rest[1].role, Role::Assistant);
    }

    #[test]
    fn split_system_no_system_returns_none() {
        let msgs = vec![Message::user("hi")];
        let (sys, rest) = split_system(&msgs);
        assert!(sys.is_none());
        assert_eq!(rest.len(), 1);
    }

    #[test]
    fn graniet_client_rejects_bad_backend_at_construction() {
        let cfg = ProviderConfig {
            backend: "bogus".into(),
            model: "x".into(),
            api_key: None,
            api_key_env: None,
            base_url: None,
            max_tokens: None,
            timeout_secs: None,
        };
        assert!(matches!(GraniteLlmClient::new(cfg), Err(LlmError::UnknownBackend(_))));
    }

    #[test]
    fn graniet_client_accepts_known_backend() {
        let cfg = ProviderConfig {
            backend: "anthropic".into(),
            model: "claude-sonnet-4-7".into(),
            api_key: None,
            api_key_env: Some("ANTHROPIC_API_KEY".into()),
            base_url: None,
            max_tokens: Some(4096),
            timeout_secs: Some(60),
        };
        assert!(GraniteLlmClient::new(cfg).is_ok());
    }

    #[tokio::test]
    async fn mock_chat_returns_canned() {
        let client = MockLlmClient::new("hello world");
        let out = client.chat(&[Message::user("hi")]).await.unwrap();
        assert_eq!(out, "hello world");
    }

    #[tokio::test]
    async fn mock_chat_stream_emits_chunks_assembling_to_response() {
        let client = MockLlmClient::new("the quick brown fox jumps");
        let mut s = client.chat_stream(&[Message::user("hi")]).await.unwrap();
        let mut acc = String::new();
        while let Some(chunk) = s.next().await {
            acc.push_str(&chunk.unwrap());
        }
        assert_eq!(acc, "the quick brown fox jumps");
    }
}
