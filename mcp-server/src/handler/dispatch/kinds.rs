//! Registry-generated kind tools: a typed `create_<kind>` / `update_<kind>`
//! write pair per registered kind, with param schemas derived from the
//! kind's field schema (`docs/kinds.md`). Strict at the tool boundary
//! (`invalid_params` for an out-of-enum value or malformed date) even
//! though on-disk validation is lenient; writes ride the same staged /
//! review-mode path the hand-written write tools use.
//
// status: mcp-registry-tools

use futures::FutureExt;
use hiker_core::frontmatter;
use hiker_core::kinds::{Field, FieldType, Kind, Registry};
use hiker_core::ops;
use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{CallToolResult, ErrorCode, ErrorData, Tool};

use super::App;
use crate::handler::params::{
    audit_err, audit_status, hiker_err, structured, translate_hiker_err, WriteOutcome, CLIENT_ID,
};

/// Which half of a kind's generated write pair a call targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::handler) enum KindOp {
    Create,
    Update,
}

impl KindOp {
    const fn prefix(self) -> &'static str {
        match self {
            Self::Create => "create_",
            Self::Update => "update_",
        }
    }

    /// `create_<kind>` / `update_<kind>` — the advertised tool name.
    fn tool_name(self, kind: &str) -> String {
        format!("{}{kind}", self.prefix())
    }
}

/// One dynamically-built route per generated tool, ready to merge into the
/// rmcp router so the pair advertises like every hand-written sibling.
/// Regenerated whenever a handler is constructed from the loaded registry.
pub(in crate::handler) fn kind_tool_routes(registry: &Registry) -> Vec<ToolRoute<App>> {
    let mut routes = Vec::with_capacity(registry.len() * 2);
    for kind in registry.iter() {
        for op in [KindOp::Create, KindOp::Update] {
            routes.push(route_for(kind, op));
        }
    }
    routes
}

fn route_for(kind: &Kind, op: KindOp) -> ToolRoute<App> {
    let attr = tool_attr(kind, op);
    let kind_name = kind.name.clone();
    ToolRoute::new_dyn(attr, move |ctx: ToolCallContext<'_, App>| {
        let kind_name = kind_name.clone();
        async move {
            let args =
                serde_json::Value::Object(ctx.arguments.clone().unwrap_or_default());
            let r = ctx.service.run_kind_tool(&kind_name, op, &args).await;
            ctx.service
                .state
                .audit
                .record(&ctx.name, &args, audit_status(&r), audit_err(&r));
            r
        }
        .boxed()
    })
}

/// The advertised `Tool` shape: name, staged-write-aware description, and
/// the JSON input schema generated from the kind's field schema.
fn tool_attr(kind: &Kind, op: KindOp) -> Tool {
    let name = op.tool_name(&kind.name);
    let description = match op {
        KindOp::Create => format!(
            "Create a note of kind `{0}` (hiker.kind: {0}) at rel_path with the given typed \
             frontmatter fields and optional body. Errors if the path already exists \
             (use update_{0}). NOTE: in review-required mode the new note is STAGED as a \
             pending proposal (status=\"staged\" + proposal_id); disk is unchanged until \
             the user accepts.",
            kind.name,
        ),
        KindOp::Update => format!(
            "Merge typed frontmatter fields into an existing note of kind `{0}`. Errors \
             if the target's hiker.kind is not `{0}` (never silently retypes a note). \
             NOTE: in review-required mode the merged result is STAGED as a pending \
             proposal (status=\"staged\" + proposal_id); disk is unchanged until the \
             user accepts.",
            kind.name,
        ),
    };
    Tool::new(name, description, input_schema(kind, op))
}

/// `{ type: object, properties, required, additionalProperties: false }` —
/// rel_path + (create-only) body + one typed property per declared field;
/// create requires the kind's `required` fields.
fn input_schema(kind: &Kind, op: KindOp) -> serde_json::Map<String, serde_json::Value> {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "rel_path".into(),
        serde_json::json!({ "type": "string", "description": "vault-relative note path" }),
    );
    let mut required = vec![serde_json::Value::String("rel_path".into())];
    if op == KindOp::Create {
        properties.insert(
            "body".into(),
            serde_json::json!({ "type": "string", "description": "markdown body (optional)" }),
        );
    }
    for field in &kind.fields {
        properties.insert(field.name.clone(), field_schema(field));
        if op == KindOp::Create && field.required {
            required.push(serde_json::Value::String(field.name.clone()));
        }
    }
    let mut schema = serde_json::Map::new();
    schema.insert("type".into(), "object".into());
    schema.insert("properties".into(), serde_json::Value::Object(properties));
    schema.insert("required".into(), serde_json::Value::Array(required));
    schema.insert("additionalProperties".into(), serde_json::Value::Bool(false));
    schema
}

/// One field's JSON-schema fragment: `number` -> number, `enum` -> an enum
/// of the declared values, `date` -> ISO-8601 string, `ref` -> a
/// vault-relative path string.
fn field_schema(field: &Field) -> serde_json::Value {
    match field.field_type {
        FieldType::String => serde_json::json!({ "type": "string" }),
        FieldType::Number => serde_json::json!({ "type": "number" }),
        FieldType::Date => serde_json::json!({
            "type": "string",
            "format": "date",
            "description": "ISO-8601 date (YYYY-MM-DD, optional time)",
        }),
        FieldType::Enum => serde_json::json!({ "type": "string", "enum": field.values }),
        FieldType::Ref => serde_json::json!({
            "type": "string",
            "description": "vault-relative path of the referenced note",
        }),
    }
}

/// Boundary-validated arguments of one generated-tool call.
struct KindArgs {
    rel_path: String,
    body: Option<String>,
    fields: serde_json::Map<String, serde_json::Value>,
}

/// Strict boundary validation: unknown params, out-of-enum values, and
/// malformed dates are `invalid_params` — the boundary is where strictness
/// costs nothing, even though on-disk validation stays lenient.
fn parse_kind_args(
    kind: &Kind,
    op: KindOp,
    args: &serde_json::Value,
) -> Result<KindArgs, ErrorData> {
    let serde_json::Value::Object(map) = args else {
        return Err(ErrorData::invalid_params("arguments must be an object", None));
    };
    let mut rel_path = None;
    let mut body = None;
    let mut fields = serde_json::Map::new();
    for (key, value) in map {
        // `rel_path` is structural — it is the note's path/identity, not
        // content, so it can never be a frontmatter field and stays reserved
        // unconditionally. `body`, by contrast, is just the markdown body and
        // a kind may legitimately declare a field literally named `body`; in
        // that case the declared field wins (so `create_<kind>` can set it)
        // rather than the body param swallowing it.
        match key.as_str() {
            "rel_path" => {
                rel_path = Some(
                    value
                        .as_str()
                        .ok_or_else(|| {
                            ErrorData::invalid_params("`rel_path` must be a string", None)
                        })?
                        .to_string(),
                );
            }
            "body" if op == KindOp::Create && kind.field("body").is_none() => {
                body = Some(
                    value
                        .as_str()
                        .ok_or_else(|| {
                            ErrorData::invalid_params("`body` must be a string", None)
                        })?
                        .to_string(),
                );
            }
            other => {
                let field = kind.field(other).ok_or_else(|| {
                    ErrorData::invalid_params(
                        format!("unknown field `{other}` for kind `{}`", kind.name),
                        None,
                    )
                })?;
                check_field_value(field, value)?;
                fields.insert(key.clone(), value.clone());
            }
        }
    }
    let rel_path = rel_path
        .ok_or_else(|| ErrorData::invalid_params("`rel_path` is required", None))?;
    if op == KindOp::Create {
        for field in kind.fields.iter().filter(|f| f.required) {
            if !fields.contains_key(&field.name) {
                return Err(ErrorData::invalid_params(
                    format!("missing required field `{}`", field.name),
                    None,
                ));
            }
        }
    } else if fields.is_empty() {
        return Err(ErrorData::invalid_params(
            "at least one field to update is required",
            None,
        ));
    }
    Ok(KindArgs { rel_path, body, fields })
}

/// One typed param against its field primitive; `Err(invalid_params)` on a
/// shape violation.
fn check_field_value(field: &Field, value: &serde_json::Value) -> Result<(), ErrorData> {
    let bad = |detail: String| Err(ErrorData::invalid_params(detail, None));
    match field.field_type {
        FieldType::Number => {
            if !value.is_number() {
                return bad(format!("`{}` must be a number", field.name));
            }
        }
        FieldType::Date => {
            let ok = value
                .as_str()
                .and_then(frontmatter::iso_date_epoch)
                .is_some();
            if !ok {
                return bad(format!("`{}` must be an ISO-8601 date string", field.name));
            }
        }
        FieldType::Enum => {
            let ok = value
                .as_str()
                .is_some_and(|v| field.values.iter().any(|allowed| allowed == v));
            if !ok {
                return bad(format!(
                    "`{}` must be one of: {}",
                    field.name,
                    field.values.join(", "),
                ));
            }
        }
        FieldType::String | FieldType::Ref => {
            if !value.is_string() {
                return bad(format!("`{}` must be a string", field.name));
            }
        }
    }
    Ok(())
}

impl App {
    /// Family gate for the generated tools: one `kind_tools_enabled`
    /// toggle plus the `writes_enabled` master gate — no per-kind config
    /// keys (the toggle table is a closed strict-load struct).
    fn guard_kind_tools(&self) -> Result<(), ErrorData> {
        let cfg = self
            .state
            .tools
            .read()
            .map_err(|_| hiker_err(ErrorCode(1004), "mcp tools cfg poisoned"))?;
        if cfg.writes_enabled && cfg.kind_tools_enabled {
            Ok(())
        } else {
            Err(hiker_err(ErrorCode(1004), "kind tools are disabled"))
        }
    }

    /// The shared workhorse behind the rmcp kind-tool routes: gate,
    /// boundary-validate, then create or update through the standard
    /// agent-write path (staged when review mode is on).
    pub(in crate::handler) async fn run_kind_tool(
        &self,
        kind_name: &str,
        op: KindOp,
        args: &serde_json::Value,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_kind_tools()?;
        let kind = self.state.kinds.get(kind_name).ok_or_else(|| {
            ErrorData::invalid_params(format!("unknown kind `{kind_name}`"), None)
        })?;
        let parsed = parse_kind_args(kind, op, args)?;
        match op {
            KindOp::Create => self.create_kind_note(kind, parsed).await,
            KindOp::Update => self.update_kind_note(kind, parsed).await,
        }
    }

    /// `create_<kind>`: compose `hiker.kind` + the typed fields into fresh
    /// frontmatter over the optional body, then ride `write_note`'s exact
    /// staged / direct branches (author stamping on create included).
    async fn create_kind_note(
        &self,
        kind: &Kind,
        args: KindArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let abs = self
            .state
            .vault
            .abs_path(&args.rel_path)
            .map_err(translate_hiker_err)?;
        if abs.exists() {
            return Err(ErrorData::invalid_params(
                format!(
                    "note already exists: {} (use update_{})",
                    args.rel_path, kind.name,
                ),
                None,
            ));
        }
        let mut patch = args.fields;
        patch.insert(
            "hiker".into(),
            serde_json::json!({ "kind": kind.name }),
        );
        let content = frontmatter::merge_agent_patch(
            args.body.as_deref().unwrap_or(""),
            serde_json::Value::Object(patch),
        )
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        self.write_kind_result(&args.rel_path, &content, None).await
    }

    /// `update_<kind>`: refuse a target whose `hiker.kind` doesn't match
    /// (never silently retype a note), then merge the typed fields through
    /// the frontmatter-merge path `set_frontmatter` uses.
    async fn update_kind_note(
        &self,
        kind: &Kind,
        args: KindArgs,
    ) -> Result<CallToolResult, ErrorData> {
        let abs = self
            .state
            .vault
            .abs_path(&args.rel_path)
            .map_err(translate_hiker_err)?;
        if !abs.exists() {
            return Err(hiker_err(
                ErrorCode(1002),
                format!("note not found: {}", args.rel_path),
            ));
        }
        let existing = self
            .state
            .vault
            .read_file(&args.rel_path)
            .map_err(translate_hiker_err)?;
        let target_kind = frontmatter::split(&existing)
            .frontmatter
            .as_ref()
            .and_then(|fm| fm.get("hiker"))
            .and_then(|h| h.get("kind"))
            .and_then(serde_yml::Value::as_str)
            .map(ToString::to_string);
        if target_kind.as_deref() != Some(kind.name.as_str()) {
            return Err(ErrorData::invalid_params(
                format!(
                    "{} is kind `{}`, not `{}` — refusing to retype it",
                    args.rel_path,
                    target_kind.as_deref().unwrap_or("<none>"),
                    kind.name,
                ),
                None,
            ));
        }
        self.write_kind_result(&args.rel_path, &existing, Some(args.fields))
            .await
    }

    /// Shared write tail for both ops: in review mode, stage the resulting
    /// whole body as one layered-doc pending proposal (the path every other
    /// write tool stages through); in direct mode, route through
    /// `core::ops::agent` so watcher suppression and
    /// re-index all behave like `write_note` / `set_frontmatter`.
    async fn write_kind_result(
        &self,
        rel_path: &str,
        content: &str,
        update_fields: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<CallToolResult, ErrorData> {
        // status: staging-review-pending-response
        let review_required = self
            .state
            .tools
            .read()
            .map(|cfg| cfg.review_required)
            .unwrap_or(false);
        if review_required {
            let layered = self.state.layered.as_ref().ok_or_else(|| {
                ErrorData::internal_error("review mode requires an open op log".to_string(), None)
            })?;
            let staged_body = match &update_fields {
                Some(fields) => frontmatter::merge_agent_patch(
                    content,
                    serde_json::Value::Object(fields.clone()),
                )
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?,
                None => content.to_string(),
            };
            let proposal_id = self.stage_whole_body(layered, rel_path, &staged_body)?;
            return Ok(structured(
                serde_json::to_value(WriteOutcome {
                    rel_path: rel_path.to_string(),
                    content_hash: String::new(),
                    status: Some("staged".into()),
                    proposal_id,
                })
                .unwrap_or(serde_json::Value::Null),
            ));
        }
        let ctx = ops::agent::WriteCtx {
            watcher: &self.state.watcher,
            jobs: &self.state.jobs,
            vault: &self.state.vault,
            layered: self.state.layered.as_ref(),
            client_id: CLIENT_ID,
        };
        let new_hash = match update_fields {
            Some(fields) => {
                ops::agent::set_frontmatter(&ctx, rel_path, serde_json::Value::Object(fields))
                    .await
            }
            None => ops::agent::write_note(&ctx, rel_path, content, None).await,
        }
        .map_err(translate_hiker_err)?;
        Ok(structured(
            serde_json::to_value(WriteOutcome {
                rel_path: rel_path.to_string(),
                content_hash: new_hash,
                status: None,
                proposal_id: None,
            })
            .unwrap_or(serde_json::Value::Null),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_kind_args, KindOp};
    use hiker_core::kinds::{Field, FieldType, Kind, Shape};

    fn field(name: &str) -> Field {
        Field {
            name: name.to_string(),
            field_type: FieldType::String,
            required: false,
            values: Vec::new(),
            ref_kind: None,
        }
    }

    fn kind_with(fields: Vec<Field>) -> Kind {
        Kind {
            name: "thing".into(),
            shape: Shape::Leaf,
            fields,
            states: Vec::new(),
            columns: Default::default(),
        }
    }

    #[test]
    fn declared_body_field_wins_over_reserved_body_param() {
        // A kind that declares a field literally named `body` must be able
        // to set it via create_<kind> — the reserved body param must not
        // swallow it.
        let kind = kind_with(vec![field("body")]);
        let args = serde_json::json!({ "rel_path": "n.md", "body": "hello" });
        let parsed = parse_kind_args(&kind, KindOp::Create, &args).expect("parse");
        assert_eq!(parsed.rel_path, "n.md");
        // The value landed in the declared field, not the body slot.
        assert_eq!(
            parsed.fields.get("body").and_then(|v| v.as_str()),
            Some("hello")
        );
        assert!(parsed.body.is_none(), "declared field should win over reserved body");
    }

    #[test]
    fn reserved_body_still_works_when_no_body_field() {
        let kind = kind_with(vec![field("note")]);
        let args = serde_json::json!({ "rel_path": "n.md", "body": "hello", "note": "x" });
        let parsed = parse_kind_args(&kind, KindOp::Create, &args).expect("parse");
        assert_eq!(parsed.body.as_deref(), Some("hello"));
        assert!(parsed.fields.get("body").is_none());
    }
}
