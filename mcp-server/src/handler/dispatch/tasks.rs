use hiker_core::tasks::types::{
    McpClientVia, Priority as TaskPriority, QueueError, TaskShape as TaskShapeKind, TaskState,
};
use rmcp::model::{CallToolResult, ErrorCode, ErrorData};

use super::App;
use crate::handler::params::{
    hiker_err, structured, TaskCheckout, TaskFail, TaskHeartbeat, TaskList, TaskSubmit,
    CLIENT_ID,
};

impl App {
    pub(in crate::handler) async fn lease_task(
        &self,
        p: &TaskCheckout,
        via: McpClientVia,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tasks()?;
        self.guard_tool("task_checkout")?;
        let lease_secs = p
            .lease_secs
            .unwrap_or(self.state.default_lease_secs)
            .min(self.state.max_lease_secs)
            .max(1);
        let shapes: Option<Vec<TaskShapeKind>> = p
            .shapes
            .as_ref()
            .map(|v| v.iter().map(|s| (*s).into()).collect());
        let min_priority: TaskPriority = p
            .min_priority
            .as_ref()
            .map(|m| (*m).into())
            .unwrap_or(TaskPriority::Low);
        let kinds: Option<&[String]> = p.types.as_deref();

        let task = self
            .state
            .tasks
            .checkout_mcp(
                CLIENT_ID,
                via,
                kinds,
                shapes.as_deref(),
                min_priority,
                lease_secs,
            )
            .await;
        let payload = match task {
            None => serde_json::Value::Null,
            Some(t) => {
                let lease_expires_ms = (std::time::SystemTime::now()
                    + std::time::Duration::from_secs(lease_secs))
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
                serde_json::json!({
                    "task_id": t.id,
                    "kind": t.kind,
                    "shape": t.shape,
                    "priority": t.priority,
                    "payload": t.payload,
                    "output_schema": t.output_schema,
                    "metadata": t.metadata,
                    "lease_expires_at_ms": lease_expires_ms,
                })
            }
        };
        Ok(structured(payload))
    }

    pub(in crate::handler) async fn record_task_result(
        &self,
        p: &TaskSubmit,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tasks()?;
        self.guard_tool("task_submit")?;
        match self
            .state
            .tasks
            .submit_result(&p.task_id, p.value.clone())
            .await
        {
            Ok(()) => Ok(structured(serde_json::json!({"ok": true}))),
            Err(QueueError::SchemaViolation(msg)) => {
                Err(hiker_err(ErrorCode(1007), format!("schema_violation: {msg}")))
            }
            Err(QueueError::StaleLease) => {
                Err(hiker_err(ErrorCode(1006), "stale_lease"))
            }
            Err(QueueError::NotFound(id)) => Err(hiker_err(
                ErrorCode(1006),
                format!("stale_lease: task not found: {id}"),
            )),
            Err(QueueError::InvalidState(s)) => {
                Err(ErrorData::internal_error(s, None))
            }
        }
    }

    pub(in crate::handler) async fn record_task_failure(
        &self,
        p: &TaskFail,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tasks()?;
        self.guard_tool("task_fail")?;
        match self
            .state
            .tasks
            .fail(&p.task_id, p.error.clone())
            .await
        {
            Ok(()) => Ok(structured(serde_json::json!({"ok": true}))),
            Err(QueueError::StaleLease) | Err(QueueError::NotFound(_)) => {
                Err(hiker_err(ErrorCode(1006), "stale_lease"))
            }
            Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
        }
    }

    pub(in crate::handler) async fn extend_task_lease(
        &self,
        p: &TaskHeartbeat,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tasks()?;
        self.guard_tool("task_heartbeat")?;
        let lease_secs = self.state.default_lease_secs;
        match self.state.tasks.heartbeat(&p.task_id, lease_secs).await {
            Ok(expires) => {
                let expires_ms = expires
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                Ok(structured(
                    serde_json::json!({"lease_expires_at_ms": expires_ms}),
                ))
            }
            Err(QueueError::StaleLease) | Err(QueueError::NotFound(_)) => {
                Err(hiker_err(ErrorCode(1006), "stale_lease"))
            }
            Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
        }
    }

    pub(in crate::handler) async fn list_tasks(&self, p: &TaskList) -> Result<CallToolResult, ErrorData> {
        self.guard_tasks()?;
        self.guard_tool("task_list")?;
        let states: Option<Vec<TaskState>> = p
            .states
            .as_ref()
            .map(|v| v.iter().map(|s| (*s).into()).collect());
        let rows = self
            .state
            .tasks
            .list(states.as_deref(), p.types.as_deref())
            .await;
        Ok(structured(
            serde_json::to_value(&rows).unwrap_or(serde_json::Value::Null),
        ))
    }
}
