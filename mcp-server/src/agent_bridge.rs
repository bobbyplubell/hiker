//! Bridge between the basic agent loop (`core::agent`) and the in-process
//! MCP server. Implements `core::agent::ToolDispatcher` against a shared
//! `HikerHandler` so the agent loop, the rmcp tool router, and the future
//! ACP path all see one tool registry, one audit log, one error model.
//
// status: agent-tool-routing-via-mcp

use std::sync::Arc;

use async_trait::async_trait;
use hiker_core::agent::{ToolDispatchError, ToolDispatcher};
use hiker_core::llm::ToolDef;

use crate::handler::HikerHandler;

/// `ToolDispatcher` impl that hands every call back through
/// `HikerHandler::dispatch_tool` — the same code path the rmcp router
/// exercises (so audit-log + error-code behaviors stay identical).
pub struct McpAgentDispatcher {
    handler: Arc<HikerHandler>,
}

impl McpAgentDispatcher {
    pub fn new(handler: Arc<HikerHandler>) -> Self {
        Self { handler }
    }
}

#[async_trait]
impl ToolDispatcher for McpAgentDispatcher {
    async fn dispatch(
        &self,
        name: &str,
        arguments_json: &str,
    ) -> Result<String, ToolDispatchError> {
        // The agent loop expects a string (typically JSON-encoded) it can
        // feed back into the model's context. `dispatch_tool` already
        // produces that shape.
        self.handler
            .dispatch_tool(name, arguments_json)
            .await
            .map_err(|e| {
                // Map by the MCP error prefix, otherwise fall through to
                // generic execution failure. The agent loop only really
                // distinguishes "ok / not ok" — these variants are
                // diagnostic.
                if e.contains("unknown tool") {
                    ToolDispatchError::UnknownTool(name.to_string())
                } else if e.contains("invalid arguments")
                    || e.contains("invalid params")
                    || e.contains("invalid ")
                {
                    ToolDispatchError::InvalidArgs(e)
                } else {
                    ToolDispatchError::Execution(e)
                }
            })
    }
}

/// Static list of tool defs the basic agent loop advertises to the model.
/// Mirrors the `#[tool]`-annotated methods on `HikerHandler` (the rmcp
/// router is the source of truth for *running* the tools; this list is the
/// source of truth for *advertising* them to the model).
///
/// JSON Schemas are emitted via `schemars` from the same `Params` types
/// the rmcp methods use — so the same TypeScript-side JSON shape the rmcp
/// client sees is what the agent loop sends to the LLM provider.
pub fn agent_tool_defs() -> Vec<ToolDef> {
    agent_tool_defs_with(false)
}

/// Variant that advertises the `task_*` tools when
/// `[tasks] expose_to_chat_agent = true`. The basic chat agent calls
/// this; external rmcp clients always see the full set via the rmcp
/// router's `#[tool]`-derived list.
///
/// status: task-queue-expose-to-chat-agent
pub fn agent_tool_defs_with(expose_tasks: bool) -> Vec<ToolDef> {
    agent_tool_defs_filtered(expose_tasks, None)
}

/// Variant that also applies the per-tool gates from
/// `[mcp.tools]` (status: `mcp-tool-toggles`). Tools whose flag is off
/// are skipped entirely so the model doesn't hallucinate calls to
/// disabled tools. The chat agent's `prepare_for_turn` calls this with
/// the live config; external rmcp clients still see the full set.
pub fn agent_tool_defs_filtered(
    expose_tasks: bool,
    tools_cfg: Option<&hiker_core::config::McpToolsConfig>,
) -> Vec<ToolDef> {
    use crate::handler::{
        ApplyTagParams, GetNoteParams, RelatedNotesParams, SearchNotesParams,
        SetFrontmatterParams, TaskCheckoutParams, TaskFailParams, TaskHeartbeatParams,
        TaskListParams, TaskSubmitParams, WriteNoteParams,
    };
    use schemars::schema_for;

    let allowed = |name: &str| -> bool {
        match tools_cfg {
            Some(c) => c.tool_allowed(name),
            None => true,
        }
    };

    fn schema<T: schemars::JsonSchema>() -> serde_json::Value {
        serde_json::to_value(schema_for!(T)).unwrap_or(serde_json::Value::Null)
    }

    let writes_gate_note = " (gated by [mcp.tools] writes_enabled)";

    let mut defs = vec![
        ToolDef {
            name: "search_notes".into(),
            description:
                "Hybrid search across the vault (lexical FTS5 + semantic vec). Returns lexical_hits, semantic_hits, and fused buckets."
                    .into(),
            parameters: schema::<SearchNotesParams>(),
        },
        ToolDef {
            name: "get_note".into(),
            description:
                "Fetch a note by vault-relative path. detail=digest|snippet|full controls the payload size."
                    .into(),
            parameters: schema::<GetNoteParams>(),
        },
        ToolDef {
            name: "related_notes".into(),
            description:
                "Top-K notes whose chunks are most semantically similar to a given note."
                    .into(),
            parameters: schema::<RelatedNotesParams>(),
        },
        ToolDef {
            name: "write_note".into(),
            description: format!(
                "Create or replace a note's body. Returns the new content hash.{writes_gate_note}"
            ),
            parameters: schema::<WriteNoteParams>(),
        },
        ToolDef {
            name: "set_frontmatter".into(),
            description: format!(
                "Deep-merge fields into a note's YAML frontmatter (auto-stamps hiker.author=agent-authored).{writes_gate_note}"
            ),
            parameters: schema::<SetFrontmatterParams>(),
        },
        ToolDef {
            name: "apply_tag".into(),
            description: format!(
                "Append a tag to a note's tags frontmatter list (idempotent).{writes_gate_note}"
            ),
            parameters: schema::<ApplyTagParams>(),
        },
        ToolDef {
            name: "remove_tag".into(),
            description: format!(
                "Remove a tag from a note's tags frontmatter list (no-op if absent).{writes_gate_note}"
            ),
            parameters: schema::<ApplyTagParams>(),
        },
    ];
    if expose_tasks {
        defs.extend([
            ToolDef {
                name: "task_checkout".into(),
                description: "Take the next eligible task from the work queue, or null if none. Stamps a lease.".into(),
                parameters: schema::<TaskCheckoutParams>(),
            },
            ToolDef {
                name: "task_submit".into(),
                description: "Submit the result for a leased task. Validates against output_schema if any.".into(),
                parameters: schema::<TaskSubmitParams>(),
            },
            ToolDef {
                name: "task_fail".into(),
                description: "Mark a leased task as failed (no auto-requeue).".into(),
                parameters: schema::<TaskFailParams>(),
            },
            ToolDef {
                name: "task_heartbeat".into(),
                description: "Extend the current lease on a leased task.".into(),
                parameters: schema::<TaskHeartbeatParams>(),
            },
            ToolDef {
                name: "task_list".into(),
                description: "List current tasks (read-only) with optional state/kind filters.".into(),
                parameters: schema::<TaskListParams>(),
            },
        ]);
    }
    // status: mcp-tool-toggles
    // Apply per-tool gates to the advertised list. Tools whose flag is
    // false get dropped silently — the model never sees them, so it
    // can't be tempted to call something that would 1004 anyway.
    defs.retain(|d| allowed(&d.name));
    defs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_tool_defs_are_complete_and_well_shaped() {
        let defs = agent_tool_defs();
        let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "search_notes",
                "get_note",
                "related_notes",
                "write_note",
                "set_frontmatter",
                "apply_tag",
                "remove_tag",
            ]
        );
        for d in &defs {
            assert!(!d.description.is_empty(), "{} missing description", d.name);
            assert!(d.parameters.is_object(), "{} schema not an object", d.name);
        }
    }
}
