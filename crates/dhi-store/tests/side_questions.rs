//! Store coverage for side-question sessions, their frozen snapshots, the
//! single-open-session rule, request appends, and task cascade.

use dhi_core::{GraphStore, NewSideQuestionRequest, SideQuestionSessionStatus, SideQuestionStatus};
use dhi_store::Store;
use sqlx::PgPool;
use uuid::Uuid;

fn ok_request(session_id: Uuid, question: &str) -> NewSideQuestionRequest {
    NewSideQuestionRequest {
        session_id,
        question: question.to_string(),
        answer: Some(format!("advisory answer to {question}")),
        status: SideQuestionStatus::Ok,
        error: None,
        model: Some("test-model".to_string()),
        tokens_in: Some(12),
        tokens_out: Some(34),
        latency_ms: Some(56),
    }
}

#[sqlx::test]
async fn side_question_roundtrip_closes_the_previous_session(pool: PgPool) {
    let store = Store::new(pool);
    let task = GraphStore::create_task(&store, "ask about it".into())
        .await
        .unwrap();

    let first = store
        .create_side_question_session(task.id, serde_json::json!({"facts": ["first"]}))
        .await
        .unwrap();
    assert_eq!(first.status, SideQuestionSessionStatus::Open);
    assert_eq!(first.snapshot["facts"][0], "first");

    let request = store
        .append_side_question(ok_request(first.id, "why?"))
        .await
        .unwrap();
    assert_eq!(request.status, SideQuestionStatus::Ok);
    assert_eq!(request.session_id, first.id);
    assert_eq!(request.tokens_in, Some(12));
    assert!(request.completed_at.is_some());

    let second = store
        .create_side_question_session(task.id, serde_json::json!({"facts": ["second"]}))
        .await
        .unwrap();
    assert_ne!(first.id, second.id);

    let stored_first = store
        .get_side_question_session(first.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored_first.status, SideQuestionSessionStatus::Closed);
    assert!(stored_first.closed_at.is_some());
    // Closing a session never rewrites its frozen snapshot.
    assert_eq!(stored_first.snapshot["facts"][0], "first");

    let open = store
        .get_open_side_question_session(task.id)
        .await
        .unwrap()
        .expect("the newest session is open");
    assert_eq!(open.id, second.id);

    store
        .append_side_question(NewSideQuestionRequest {
            session_id: second.id,
            question: "failed one".to_string(),
            answer: None,
            status: SideQuestionStatus::Error,
            error: Some("provider down".to_string()),
            model: None,
            tokens_in: None,
            tokens_out: None,
            latency_ms: Some(2),
        })
        .await
        .unwrap();

    let threads = store.list_side_question_sessions(task.id).await.unwrap();
    assert_eq!(threads.len(), 2);
    assert_eq!(threads[0].session.id, second.id, "newest first");
    assert_eq!(threads[0].requests.len(), 1);
    assert_eq!(threads[0].requests[0].status, SideQuestionStatus::Error);
    assert_eq!(threads[1].session.id, first.id);
    assert_eq!(threads[1].requests.len(), 1);
    assert_eq!(
        threads[1].requests[0].answer.as_deref(),
        Some("advisory answer to why?")
    );
}

#[sqlx::test]
async fn deleting_a_task_cascades_side_question_sessions_and_requests(pool: PgPool) {
    let store = Store::new(pool);
    let task = GraphStore::create_task(&store, "cascade".into())
        .await
        .unwrap();
    let session = store
        .create_side_question_session(task.id, serde_json::json!({"task": task.id}))
        .await
        .unwrap();
    store
        .append_side_question(ok_request(session.id, "will this cascade?"))
        .await
        .unwrap();

    sqlx::query("DELETE FROM tasks WHERE id = $1")
        .bind(task.id)
        .execute(store.pool())
        .await
        .unwrap();

    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM side_question_sessions")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let requests: i64 = sqlx::query_scalar("SELECT count(*) FROM side_question_requests")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(sessions, 0);
    assert_eq!(requests, 0);
}
