use std::time::{Duration, SystemTime};

use super::queue::{validate_against_schema, Queue, WorkerPreferenceCfg};
use super::types::{
    CancelReason, McpClientVia, Priority, QueueError, Task, TaskKind, TaskOutcome, TaskPayload,
    TaskShape,
};
use crate::config::TasksConfig;

fn task(kind: TaskKind, priority: Priority, shape: TaskShape) -> Task {
    Task {
        id: ulid::Ulid::new().to_string(),
        kind,
        priority,
        shape,
        payload: TaskPayload {
            prompt: "test prompt".into(),
            inputs: serde_json::Value::Null,
        },
        output_schema: None,
        submitted_at: SystemTime::now(),
        metadata: serde_json::Value::Null,
    }
}

#[tokio::test]
async fn submit_and_complete_round_trip() {
    let cfg = TasksConfig {
        worker_preference: WorkerPreferenceCfg::Internal,
        ..Default::default()
    };
    let q = Queue::new(cfg);
    let handle = q
        .submit(task(
            TaskKind::AutoTag { source_path: "a.md".into() },
            Priority::Normal,
            TaskShape::Direct,
        ))
        .await;
    let id = handle.id.clone();
    let (taken, _stop) = q.checkout_direct(60).await.expect("eligible");
    assert_eq!(taken.id, id);
    q.submit_result(&id, serde_json::json!({"tag": "ok"}))
        .await
        .unwrap();
    match handle.await_outcome().await {
        TaskOutcome::Completed { value, .. } => assert_eq!(value["tag"], "ok"),
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
async fn cancel_queued_task_resolves_handle() {
    let q = Queue::new(TasksConfig::default());
    let handle = q
        .submit(task(
            TaskKind::AutoTag { source_path: "a.md".into() },
            Priority::Normal,
            TaskShape::Direct,
        ))
        .await;
    let id = handle.id.clone();
    q.cancel(&id).await;
    match handle.await_outcome().await {
        TaskOutcome::Cancelled { reason } => {
            assert_eq!(reason, CancelReason::UserAction);
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
}

#[tokio::test]
async fn priority_ordering_high_drains_first() {
    let cfg = TasksConfig {
        worker_preference: WorkerPreferenceCfg::Internal,
        ..Default::default()
    };
    let q = Queue::new(cfg);
    let _low = q
        .submit(task(
            TaskKind::AutoTag { source_path: "low.md".into() },
            Priority::Low,
            TaskShape::Direct,
        ))
        .await;
    // Different submitted_at to keep ordering deterministic.
    tokio::time::sleep(Duration::from_millis(2)).await;
    let high_handle = q
        .submit(task(
            TaskKind::AutoTag { source_path: "hi.md".into() },
            Priority::High,
            TaskShape::Direct,
        ))
        .await;
    let (taken, _) = q.checkout_direct(60).await.unwrap();
    assert_eq!(taken.id, high_handle.id);
}

#[tokio::test]
async fn agent_shape_skipped_by_direct_worker() {
    let cfg = TasksConfig {
        worker_preference: WorkerPreferenceCfg::Internal,
        ..Default::default()
    };
    let q = Queue::new(cfg);
    let _ = q
        .submit(task(
            TaskKind::NoteMutation {
                mutation: "rewrite".into(),
                source_path: "n.md".into(),
            },
            Priority::High,
            TaskShape::Agent,
        ))
        .await;
    assert!(q.checkout_direct(60).await.is_none());
}

#[tokio::test]
async fn schema_violation_rejects_submit() {
    let q = Queue::new(TasksConfig {
        worker_preference: WorkerPreferenceCfg::Internal,
        ..Default::default()
    });
    let mut t = task(
        TaskKind::AutoTag { source_path: "a.md".into() },
        Priority::Normal,
        TaskShape::Direct,
    );
    t.output_schema = Some(serde_json::json!({
        "type": "object",
        "required": ["tag"],
    }));
    let handle = q.submit(t).await;
    let id = handle.id.clone();
    let _ = q.checkout_direct(60).await.unwrap();
    let err = q
        .submit_result(&id, serde_json::json!({"wrong": 1}))
        .await
        .unwrap_err();
    match err {
        QueueError::SchemaViolation(_) => {}
        other => panic!("expected SchemaViolation, got {other:?}"),
    }
}

#[tokio::test]
async fn mcp_checkout_filters_min_priority() {
    let q = Queue::new(TasksConfig {
        worker_preference: WorkerPreferenceCfg::External,
        ..Default::default()
    });
    let _low = q
        .submit(task(
            TaskKind::AutoTag { source_path: "lo.md".into() },
            Priority::Low,
            TaskShape::Direct,
        ))
        .await;
    let normal = q
        .submit(task(
            TaskKind::AutoTag { source_path: "no.md".into() },
            Priority::Normal,
            TaskShape::Direct,
        ))
        .await;
    let taken = q
        .checkout_mcp(
            "client-a",
            McpClientVia::External,
            None,
            None,
            Priority::Normal,
            60,
        )
        .await
        .unwrap();
    assert_eq!(taken.id, normal.id);
}

#[tokio::test]
async fn lease_expiry_requeues_via_tick() {
    let q = Queue::new(TasksConfig {
        worker_preference: WorkerPreferenceCfg::Internal,
        ..Default::default()
    });
    let h = q
        .submit(task(
            TaskKind::AutoTag { source_path: "a.md".into() },
            Priority::Normal,
            TaskShape::Direct,
        ))
        .await;
    let _ = q.checkout_mcp(
        "ext", McpClientVia::External, None, None, Priority::Low, 0).await.unwrap();
    // Lease secs = 0 → expired immediately. Tick should requeue.
    tokio::time::sleep(Duration::from_millis(20)).await;
    q.tick_maintenance().await;
    // Now direct worker should be able to take it.
    let (again, _) = q.checkout_direct(60).await.expect("requeued");
    assert_eq!(again.id, h.id);
}

#[test]
fn schema_validates_top_level_type() {
    let s = serde_json::json!({"type": "object"});
    assert!(validate_against_schema(&s, &serde_json::json!({})).is_ok());
    assert!(validate_against_schema(&s, &serde_json::json!([])).is_err());
}
