use std::sync::Arc;

use aionui_ai_agent::{AgentError, IWorkerTaskManager};
use aionui_api_types::{
    CloneConversationRequest, CreateConversationRequest, ListMessagesQuery, SearchMessagesQuery, WebSocketMessage,
};
use aionui_common::{AgentKillReason, ConversationStatus, TimestampMs, generate_prefixed_id, now_ms};
use aionui_conversation::skill_resolver::SkillResolver;
use aionui_conversation::{ConversationError, ConversationService};
use aionui_db::models::MessageRow;
use aionui_db::{IConversationRepository, SqliteConversationRepository, init_database_memory};
use aionui_realtime::EventBroadcaster;
use serde_json::json;
use std::sync::Mutex;

// ── Test infrastructure ────────────────────────────────────────────

struct TestBroadcaster {
    events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
}

impl TestBroadcaster {
    fn new() -> Self {
        Self {
            events: Mutex::new(vec![]),
        }
    }
}

impl EventBroadcaster for TestBroadcaster {
    fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
        self.events.lock().unwrap().push(event);
    }
}

struct NoopTaskManager;

#[async_trait::async_trait]
impl IWorkerTaskManager for NoopTaskManager {
    fn get_task(&self, _: &str) -> Option<aionui_ai_agent::AgentInstance> {
        None
    }
    async fn get_or_build_task(
        &self,
        _: &str,
        _: aionui_ai_agent::types::BuildTaskOptions,
    ) -> Result<aionui_ai_agent::AgentInstance, AgentError> {
        Err(AgentError::internal("noop"))
    }
    fn kill(&self, _: &str, _: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }
    fn kill_and_wait(
        &self,
        _: &str,
        _: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }
    async fn clear(&self) {}
    fn active_count(&self) -> usize {
        0
    }
    fn collect_idle(&self, _: TimestampMs) -> Vec<String> {
        vec![]
    }
}

struct EmptySkillResolver;

#[async_trait::async_trait]
impl SkillResolver for EmptySkillResolver {
    async fn auto_inject_names(&self) -> Vec<String> {
        Vec::new()
    }

    async fn resolve_skills(&self, _names: &[String]) -> Vec<aionui_extension::ResolvedAgentSkill> {
        Vec::new()
    }
}

async fn setup() -> (
    ConversationService,
    Arc<SqliteConversationRepository>,
    Arc<TestBroadcaster>,
) {
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let broadcaster = Arc::new(TestBroadcaster::new());
    let agent_metadata_repo: Arc<dyn aionui_db::IAgentMetadataRepository> =
        Arc::new(aionui_db::SqliteAgentMetadataRepository::new(db.pool().clone()));
    let acp_session_repo: Arc<dyn aionui_db::IAcpSessionRepository> =
        Arc::new(aionui_db::SqliteAcpSessionRepository::new(db.pool().clone()));
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(NoopTaskManager);
    let svc = ConversationService::new(
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(EmptySkillResolver),
        task_mgr,
        repo.clone(),
        agent_metadata_repo,
        acp_session_repo,
    );
    (svc, repo, broadcaster)
}

const USER_ID: &str = "system_default_user";

fn ensure_test_workspace_path() -> String {
    ensure_named_workspace_path("aionui-conversation-extended-test-project")
}

fn ensure_named_workspace_path(name: &str) -> String {
    let workspace = std::env::temp_dir().join(name);
    std::fs::create_dir_all(&workspace).unwrap();
    workspace.to_string_lossy().to_string()
}

fn make_create_req() -> CreateConversationRequest {
    let workspace = ensure_test_workspace_path();
    serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": workspace }
    }))
    .unwrap()
}

fn make_message(conv_id: &str, content: &str, offset_ms: i64) -> MessageRow {
    MessageRow {
        id: generate_prefixed_id("msg"),
        conversation_id: conv_id.to_string(),
        msg_id: Some(generate_prefixed_id("client")),
        r#type: "text".to_string(),
        content: format!(r#"{{"content":"{content}"}}"#),
        position: Some("right".to_string()),
        status: Some("finish".to_string()),
        hidden: false,
        created_at: now_ms() + offset_ms,
        backend_turn_id: None,
    }
}

fn make_acp_tool_message(conv_id: &str, id: &str, output: &str, offset_ms: i64) -> MessageRow {
    MessageRow {
        id: id.to_string(),
        conversation_id: conv_id.to_string(),
        msg_id: Some(id.to_string()),
        r#type: "acp_tool_call".to_string(),
        content: json!({
            "session_id": "session-1",
            "update": {
                "session_update": "tool_call",
                "tool_call_id": id,
                "status": "completed",
                "title": "rg",
                "kind": "search",
                "raw_input": { "pattern": "needle", "path": "." },
                "content": [{
                    "type": "content",
                    "content": { "type": "text", "text": output }
                }]
            }
        })
        .to_string(),
        position: Some("left".to_string()),
        status: Some("finish".to_string()),
        hidden: false,
        created_at: now_ms() + offset_ms,
        backend_turn_id: None,
    }
}

// ── T6: Clone conversation ─────────────────────────────────────────

#[tokio::test]
async fn t6_2_clone_without_source() {
    let (svc, _repo, _b) = setup().await;

    let req: CloneConversationRequest = serde_json::from_value(json!({
        "conversation": {
            "type": "acp",
            "name": "Direct",
            "extra": {}
        }
    }))
    .unwrap();
    let resp = svc.clone_create(USER_ID, req).await.unwrap();
    assert_eq!(resp.name, "Direct");
    // No source to merge from — only the caller-provided CreateConversationRequest
    // drives `extra`, so source-only keys (e.g. `contextFileName`) must not appear.
    assert!(resp.extra.get("contextFileName").is_none());
}

// ── T7: Reset conversation ─────────────────────────────────────────

#[tokio::test]
async fn t7_1_reset_clears_messages_and_status() {
    let (svc, repo, _b) = setup().await;

    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    // Insert messages
    for i in 0..3 {
        repo.insert_message(USER_ID, &make_message(&conv.id, &format!("msg {i}"), i))
            .await
            .unwrap();
    }

    svc.reset(USER_ID, &conv.id).await.unwrap();

    let fetched = svc.get(USER_ID, &conv.id).await.unwrap();
    assert_eq!(fetched.status, ConversationStatus::Pending);

    let messages = svc
        .list_messages(USER_ID, &conv.id, ListMessagesQuery::default())
        .await
        .unwrap();
    assert!(messages.items.is_empty());
    assert!(messages.oldest_cursor.is_none());
    assert!(messages.newest_cursor.is_none());
    assert!(!messages.has_more_before);
    assert!(!messages.has_more_after);
}

#[tokio::test]
async fn t7_3_reset_not_found() {
    let (svc, _repo, _b) = setup().await;
    let err = svc.reset(USER_ID, "nonexistent").await.unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

// ── T8: Message list ───────────────────────────────────────────────

#[tokio::test]
async fn t8_1_empty_messages() {
    let (svc, _repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    let result = svc
        .list_messages(USER_ID, &conv.id, ListMessagesQuery::default())
        .await
        .unwrap();
    assert!(result.items.is_empty());
    assert!(result.oldest_cursor.is_none());
    assert!(result.newest_cursor.is_none());
    assert!(!result.has_more_before);
    assert!(!result.has_more_after);
}

#[tokio::test]
async fn t8_2_cursor_pagination() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    for i in 0..10 {
        repo.insert_message(USER_ID, &make_message(&conv.id, &format!("msg {i}"), i * 100))
            .await
            .unwrap();
    }

    let query = ListMessagesQuery {
        limit: Some(3),
        content_mode: None,
        ..Default::default()
    };
    let result = svc.list_messages(USER_ID, &conv.id, query).await.unwrap();
    assert_eq!(result.items.len(), 3);
    assert!(result.oldest_cursor.is_some());
    assert!(result.newest_cursor.is_some());
    assert!(result.has_more_before);
    assert!(!result.has_more_after);

    let older = svc
        .list_messages(
            USER_ID,
            &conv.id,
            ListMessagesQuery {
                limit: Some(3),
                before: result.oldest_cursor.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(older.items.len(), 3);
    assert!(older.has_more_before);
    assert!(older.has_more_after);
}

#[tokio::test]
async fn t8_3_asc_order_default() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    for i in 0..3 {
        repo.insert_message(USER_ID, &make_message(&conv.id, &format!("msg {i}"), i * 1000))
            .await
            .unwrap();
    }

    let result = svc
        .list_messages(USER_ID, &conv.id, ListMessagesQuery::default())
        .await
        .unwrap();
    // Latest page is returned in ascending display order.
    assert!(result.items[0].created_at <= result.items[1].created_at);
    assert!(result.items[1].created_at <= result.items[2].created_at);
}

#[tokio::test]
async fn t8_4_asc_order() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    for i in 0..3 {
        repo.insert_message(USER_ID, &make_message(&conv.id, &format!("msg {i}"), i * 1000))
            .await
            .unwrap();
    }

    let result = svc
        .list_messages(USER_ID, &conv.id, ListMessagesQuery::default())
        .await
        .unwrap();
    assert!(result.items[0].created_at <= result.items[1].created_at);
    assert!(result.items[1].created_at <= result.items[2].created_at);
}

#[tokio::test]
async fn t8_5_conversation_not_found() {
    let (svc, _repo, _b) = setup().await;
    let err = svc
        .list_messages(USER_ID, "nonexistent", ListMessagesQuery::default())
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

#[tokio::test]
async fn t8_6_rejects_before_and_after_together() {
    let (svc, _repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    let err = svc
        .list_messages(
            USER_ID,
            &conv.id,
            ListMessagesQuery {
                before: Some("v1.bad".into()),
                after: Some("v1.bad".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();

    assert!(
        matches!(err, ConversationError::BadRequest { reason } if reason == "before, after, and anchor_message_id are mutually exclusive")
    );
}

#[tokio::test]
async fn t8_7_rejects_anchor_with_cursor() {
    let (svc, _repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    let err = svc
        .list_messages(
            USER_ID,
            &conv.id,
            ListMessagesQuery {
                before: Some("v1.bad".into()),
                anchor_message_id: Some("msg-1".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();

    assert!(
        matches!(err, ConversationError::BadRequest { reason } if reason == "before, after, and anchor_message_id are mutually exclusive")
    );
}

#[tokio::test]
async fn t8_8_rejects_invalid_cursor() {
    let (svc, _repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    let err = svc
        .list_messages(
            USER_ID,
            &conv.id,
            ListMessagesQuery {
                before: Some("not-a-cursor".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(err, ConversationError::BadRequest { reason } if reason == "invalid message cursor"));
}

#[tokio::test]
async fn t8_9_anchor_returns_window_containing_target() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    let mut target_id = String::new();

    for i in 0..7 {
        let msg = make_message(&conv.id, &format!("msg {i}"), i * 100);
        if i == 3 {
            target_id = msg.id.clone();
        }
        repo.insert_message(USER_ID, &msg).await.unwrap();
    }

    let page = svc
        .list_messages(
            USER_ID,
            &conv.id,
            ListMessagesQuery {
                limit: Some(5),
                anchor_message_id: Some(target_id.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(page.items.len(), 5);
    assert!(page.items.iter().any(|item| item.id == target_id));
    assert!(page.has_more_before);
    assert!(page.has_more_after);
}

// ── T9: Message search ─────────────────────────────────────────────

#[tokio::test]
async fn t8_6_compact_mode_truncates_large_tool_content_only_for_list_response() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    let large_output = "match line\n".repeat(10_000);

    repo.insert_message(USER_ID, &make_acp_tool_message(&conv.id, "tool-big", &large_output, 0))
        .await
        .unwrap();

    let full = svc
        .list_messages(USER_ID, &conv.id, ListMessagesQuery::default())
        .await
        .unwrap();
    assert_eq!(
        full.items[0].content["update"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap(),
        large_output
    );

    let compact = svc
        .list_messages(
            USER_ID,
            &conv.id,
            ListMessagesQuery {
                content_mode: Some("compact".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let compact_content = &compact.items[0].content;
    let preview = compact_content["update"]["content"][0]["content"]["text"]
        .as_str()
        .unwrap();

    assert!(compact_content["_compact"]["truncated"].as_bool().unwrap());
    assert!(compact_content["_compact"]["original_size"].as_u64().unwrap() > preview.len() as u64);
    assert!(preview.len() < large_output.len());
    assert!(!preview.contains(&large_output));
}

#[tokio::test]
async fn t8_7_get_message_returns_full_tool_content_after_compact_list() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    let large_output = "wide rg output\n".repeat(10_000);

    repo.insert_message(
        USER_ID,
        &make_acp_tool_message(&conv.id, "tool-detail", &large_output, 0),
    )
    .await
    .unwrap();

    let _ = svc
        .list_messages(
            USER_ID,
            &conv.id,
            ListMessagesQuery {
                content_mode: Some("compact".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let detail = svc.get_message(USER_ID, &conv.id, "tool-detail").await.unwrap();

    assert_eq!(
        detail.content["update"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap(),
        large_output
    );
}

#[tokio::test]
async fn t8_8_get_message_not_found() {
    let (svc, _repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    let err = svc.get_message(USER_ID, &conv.id, "missing-message").await.unwrap_err();

    assert!(matches!(
        err,
        ConversationError::MessageNotFound { id } if id == "missing-message"
    ));
}

#[tokio::test]
async fn t9_1_keyword_match() {
    let (svc, repo, _b) = setup().await;
    let workspace = ensure_test_workspace_path();
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    repo.insert_message(USER_ID, &make_message(&conv.id, "Rust review report", 0))
        .await
        .unwrap();
    repo.insert_message(USER_ID, &make_message(&conv.id, "Python test", 100))
        .await
        .unwrap();

    let query = SearchMessagesQuery {
        keyword: "review".into(),
        page: None,
        page_size: None,
    };
    let result = svc.search_messages(USER_ID, query).await.unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.total, 1);

    let item = &result.items[0];
    assert_eq!(item.message_type, "text");
    assert!(item.message_created_at > 0);
    assert!(item.preview_text.contains("Rust review report"));

    assert_eq!(item.conversation.id, conv.id);
    assert_eq!(item.conversation.name, conv.name);
    assert_eq!(item.conversation.extra["workspace"], workspace);
}

#[tokio::test]
async fn t9_2_no_match() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    repo.insert_message(USER_ID, &make_message(&conv.id, "hello world", 0))
        .await
        .unwrap();

    let query = SearchMessagesQuery {
        keyword: "xxxxnotexist".into(),
        page: None,
        page_size: None,
    };
    let result = svc.search_messages(USER_ID, query).await.unwrap();
    assert!(result.items.is_empty());
    assert_eq!(result.total, 0);
}

#[tokio::test]
async fn t9_3_search_pagination() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    for i in 0..5 {
        repo.insert_message(
            USER_ID,
            &make_message(&conv.id, &format!("match keyword item {i}"), i * 100),
        )
        .await
        .unwrap();
    }

    let query = SearchMessagesQuery {
        keyword: "keyword".into(),
        page: Some(1),
        page_size: Some(2),
    };
    let result = svc.search_messages(USER_ID, query).await.unwrap();
    assert_eq!(result.items.len(), 2);
    assert_eq!(result.total, 5);
    assert!(result.has_more);
}

#[tokio::test]
async fn t9_4_empty_keyword() {
    let (svc, _repo, _b) = setup().await;

    let query = SearchMessagesQuery {
        keyword: "".into(),
        page: None,
        page_size: None,
    };
    let err = svc.search_messages(USER_ID, query).await.unwrap_err();
    assert!(matches!(err, ConversationError::BadRequest { .. }));
}

#[tokio::test]
async fn t9_5_preview_text_extracts_from_json_content() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    let complex_msg = MessageRow {
        id: generate_prefixed_id("msg"),
        conversation_id: conv.id.clone(),
        msg_id: None,
        r#type: "text".to_string(),
        content: r#"[{"type":"text","content":"Design document for search"},{"type":"text","content":"feature implementation"}]"#.to_string(),
        position: Some("right".to_string()),
        status: Some("finish".to_string()),
        hidden: false,
        created_at: now_ms(),
        backend_turn_id: None,
    };
    repo.insert_message(USER_ID, &complex_msg).await.unwrap();

    let query = SearchMessagesQuery {
        keyword: "search".into(),
        page: None,
        page_size: None,
    };
    let result = svc.search_messages(USER_ID, query).await.unwrap();
    assert_eq!(result.items.len(), 1);

    let item = &result.items[0];
    assert!(!item.preview_text.contains('{'));
    assert!(!item.preview_text.contains('['));
    assert!(item.preview_text.contains("Design document for search"));
    assert!(item.preview_text.contains("feature implementation"));
}

#[tokio::test]
async fn t9_6_search_result_includes_conversation_model() {
    let (svc, repo, _b) = setup().await;
    let workspace = ensure_test_workspace_path();

    // Search surfaces conversation.model only for aionrs (the only type that
    // carries a top-level model under the aionrs-only rule).
    let aionrs_req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "aionrs",
        "model": { "provider_id": "p1", "model": "claude-sonnet-4-20250514" },
        "extra": { "workspace": workspace }
    }))
    .unwrap();
    let conv = svc.create(USER_ID, aionrs_req).await.unwrap();

    repo.insert_message(USER_ID, &make_message(&conv.id, "model test keyword", 0))
        .await
        .unwrap();

    let query = SearchMessagesQuery {
        keyword: "model test".into(),
        page: None,
        page_size: None,
    };
    let result = svc.search_messages(USER_ID, query).await.unwrap();
    assert_eq!(result.items.len(), 1);

    let item = &result.items[0];
    let model = item.conversation.model.as_ref().unwrap();
    assert_eq!(model.provider_id, "p1");
    assert_eq!(model.model, "claude-sonnet-4-20250514");
}

#[tokio::test]
async fn t9_7_search_does_not_leak_other_users_messages() {
    let (svc, repo, _b) = setup().await;

    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    repo.insert_message(USER_ID, &make_message(&conv.id, "secret keyword data", 0))
        .await
        .unwrap();

    let query = SearchMessagesQuery {
        keyword: "secret".into(),
        page: None,
        page_size: None,
    };
    let result = svc.search_messages("other_user_id", query).await.unwrap();
    assert!(result.items.is_empty());
    assert_eq!(result.total, 0);
}

// ── T10: Associated conversations ──────────────────────────────────

#[tokio::test]
async fn t10_1_same_workspace() {
    let (svc, _repo, _b) = setup().await;
    let shared_workspace = ensure_named_workspace_path("aionui-conversation-extended-shared-workspace");
    let other_workspace = ensure_named_workspace_path("aionui-conversation-extended-other-workspace");

    let req1: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "name": "Conv A",
        "extra": { "workspace": shared_workspace }
    }))
    .unwrap();
    let conv1 = svc.create(USER_ID, req1).await.unwrap();

    let req2: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "name": "Conv B",
        "extra": { "workspace": shared_workspace }
    }))
    .unwrap();
    let conv2 = svc.create(USER_ID, req2).await.unwrap();

    // Different workspace
    let req3: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "name": "Conv C",
        "extra": { "workspace": other_workspace }
    }))
    .unwrap();
    svc.create(USER_ID, req3).await.unwrap();

    let associated = svc.list_associated(USER_ID, &conv1.id).await.unwrap();
    assert_eq!(associated.len(), 1);
    assert_eq!(associated[0].id, conv2.id);
}

#[tokio::test]
async fn t10_2_no_associated() {
    let (svc, _repo, _b) = setup().await;
    let workspace = ensure_named_workspace_path("aionui-conversation-extended-unique-workspace");

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": workspace }
    }))
    .unwrap();
    let conv = svc.create(USER_ID, req).await.unwrap();

    let associated = svc.list_associated(USER_ID, &conv.id).await.unwrap();
    assert!(associated.is_empty());
}

#[tokio::test]
async fn t10_3_associated_not_found() {
    let (svc, _repo, _b) = setup().await;
    let err = svc.list_associated(USER_ID, "nonexistent").await.unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

// ── T12: Boundary scenarios ────────────────────────────────────────

#[tokio::test]
async fn t12_4_search_sql_injection() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    repo.insert_message(USER_ID, &make_message(&conv.id, "safe content", 0))
        .await
        .unwrap();

    let query = SearchMessagesQuery {
        keyword: "'; DROP TABLE messages; --".into(),
        page: None,
        page_size: None,
    };
    // Should return empty results, not crash
    let result = svc.search_messages(USER_ID, query).await.unwrap();
    assert!(result.items.is_empty());
}

// ── Ownership cross-cutting ────────────────────────────────────────

#[tokio::test]
async fn messages_wrong_user_returns_not_found() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    repo.insert_message(USER_ID, &make_message(&conv.id, "hello", 0))
        .await
        .unwrap();

    let err = svc
        .list_messages("other_user", &conv.id, ListMessagesQuery::default())
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

#[tokio::test]
async fn reset_wrong_user_returns_not_found() {
    let (svc, _repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    let err = svc.reset("other_user", &conv.id).await.unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

// ── Conversation fork ───────────────────────────────────────────────

async fn setup_fork() -> (
    ConversationService,
    Arc<SqliteConversationRepository>,
    Arc<dyn aionui_db::IAcpSessionRepository>,
) {
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let broadcaster = Arc::new(TestBroadcaster::new());
    let agent_metadata_repo: Arc<dyn aionui_db::IAgentMetadataRepository> =
        Arc::new(aionui_db::SqliteAgentMetadataRepository::new(db.pool().clone()));
    let acp_session_repo: Arc<dyn aionui_db::IAcpSessionRepository> =
        Arc::new(aionui_db::SqliteAcpSessionRepository::new(db.pool().clone()));
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(NoopTaskManager);
    let svc = ConversationService::new(
        std::env::temp_dir(),
        broadcaster,
        Arc::new(EmptySkillResolver),
        task_mgr,
        repo.clone(),
        agent_metadata_repo,
        acp_session_repo.clone(),
    );
    (svc, repo, acp_session_repo)
}

fn fork_create_req(backend: &str) -> CreateConversationRequest {
    let workspace = ensure_named_workspace_path(&format!("aionui-fork-test-{backend}"));
    serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": workspace, "backend": backend }
    }))
    .unwrap()
}

fn fork_req(message_id: &str) -> aionui_api_types::ForkConversationRequest {
    serde_json::from_value(json!({ "message_id": message_id })).unwrap()
}

#[tokio::test]
async fn fork_head_creates_lineage_row_and_copies_messages() {
    let (svc, repo, acp_repo) = setup_fork().await;
    let parent = svc.create(USER_ID, fork_create_req("claude")).await.unwrap();
    acp_repo
        .update_session_id_for_user(USER_ID, &parent.id, "parent-sid-1")
        .await
        .unwrap();
    let m1 = make_message(&parent.id, "hello", 0);
    let m2 = make_message(&parent.id, "world", 10);
    repo.insert_message(USER_ID, &m1).await.unwrap();
    repo.insert_message(USER_ID, &m2).await.unwrap();

    // HEAD fork (claude is seeded with a head-only fork capability by 036).
    let fork = svc.fork(USER_ID, &parent.id, fork_req(&m2.id)).await.unwrap();
    assert_ne!(fork.id, parent.id);
    assert_eq!(fork.name, parent.name, "name defaults to the parent's");
    assert_eq!(
        fork.fork_capability,
        Some(aionui_api_types::ForkCapabilityView { at_turn: false }),
        "claude declares a HEAD-only fork capability"
    );
    let spec = fork.extra.get("fork").expect("fork lineage in extra");
    assert_eq!(spec["parent_conversation_id"], parent.id);
    assert_eq!(spec["parent_message_id"], m2.id);
    assert_eq!(spec["parent_session_id"], "parent-sid-1");
    assert!(spec.get("last_turn_id").is_none(), "HEAD fork carries no turn anchor");

    // Copied history, reminted ids, same workspace.
    let page = repo
        .list_messages_page(
            USER_ID,
            &fork.id,
            &aionui_db::MessagePageParams {
                limit: 50,
                direction: aionui_db::MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(
        fork.extra.get("workspace"),
        // The response extra echoes the parent's workspace verbatim.
        svc.get(USER_ID, &parent.id).await.unwrap().extra.get("workspace"),
        "fork shares the parent workspace (claude cwd constraint)"
    );

    // The fork's acp_session row exists, same agent identity, UNBOUND sid.
    let fork_session = acp_repo.get_for_user(USER_ID, &fork.id).await.unwrap().unwrap();
    assert_eq!(fork_session.session_id, None, "fork pending until first open");
    let parent_session = acp_repo.get_for_user(USER_ID, &parent.id).await.unwrap().unwrap();
    assert_eq!(fork_session.agent_id, parent_session.agent_id);
    assert_eq!(
        parent_session.session_id.as_deref(),
        Some("parent-sid-1"),
        "the parent's binding is untouched"
    );
}

#[tokio::test]
async fn fork_mid_history_is_refused_for_head_only_backends() {
    let (svc, repo, acp_repo) = setup_fork().await;
    let parent = svc.create(USER_ID, fork_create_req("claude")).await.unwrap();
    acp_repo
        .update_session_id_for_user(USER_ID, &parent.id, "parent-sid-2")
        .await
        .unwrap();
    let m1 = make_message(&parent.id, "one", 0);
    let m2 = make_message(&parent.id, "two", 10);
    repo.insert_message(USER_ID, &m1).await.unwrap();
    repo.insert_message(USER_ID, &m2).await.unwrap();

    let err = svc.fork(USER_ID, &parent.id, fork_req(&m1.id)).await.unwrap_err();
    assert!(
        matches!(&err, ConversationError::Unprocessable { reason } if reason.starts_with("FORK_POINT_UNSUPPORTED")),
        "claude mid-history fork must be refused, got {err:?}"
    );
}

#[tokio::test]
async fn fork_mid_history_resolves_codex_turn_anchor() {
    let (svc, repo, acp_repo) = setup_fork().await;
    let parent = svc.create(USER_ID, fork_create_req("codex")).await.unwrap();
    acp_repo
        .update_session_id_for_user(USER_ID, &parent.id, "th-parent")
        .await
        .unwrap();
    let mut m1 = make_message(&parent.id, "one", 0);
    m1.backend_turn_id = Some("turn-1".into());
    let m2 = make_message(&parent.id, "two", 10);
    repo.insert_message(USER_ID, &m1).await.unwrap();
    repo.insert_message(USER_ID, &m2).await.unwrap();

    let fork = svc.fork(USER_ID, &parent.id, fork_req(&m1.id)).await.unwrap();
    assert_eq!(
        fork.fork_capability,
        Some(aionui_api_types::ForkCapabilityView { at_turn: true }),
        "codex declares an at-turn fork capability"
    );
    assert_eq!(
        fork.extra["fork"]["last_turn_id"], "turn-1",
        "the mid-history fork resolved the stamped codex turn anchor"
    );
    // And only the pre-fork-point message was copied.
    let page = repo
        .list_messages_page(
            USER_ID,
            &fork.id,
            &aionui_db::MessagePageParams {
                limit: 50,
                direction: aionui_db::MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
}

#[tokio::test]
async fn fork_mid_history_without_anchor_is_refused_not_degraded() {
    let (svc, repo, acp_repo) = setup_fork().await;
    let parent = svc.create(USER_ID, fork_create_req("codex")).await.unwrap();
    acp_repo
        .update_session_id_for_user(USER_ID, &parent.id, "th-parent")
        .await
        .unwrap();
    // Legacy rows: no backend_turn_id anywhere.
    let m1 = make_message(&parent.id, "one", 0);
    let m2 = make_message(&parent.id, "two", 10);
    repo.insert_message(USER_ID, &m1).await.unwrap();
    repo.insert_message(USER_ID, &m2).await.unwrap();

    let err = svc.fork(USER_ID, &parent.id, fork_req(&m1.id)).await.unwrap_err();
    assert!(
        matches!(&err, ConversationError::Unprocessable { reason } if reason.starts_with("FORK_POINT_UNSUPPORTED")),
        "an unresolvable anchor must refuse, never silently fork at HEAD, got {err:?}"
    );
}

#[tokio::test]
async fn fork_uses_handoff_for_unbound_parent_and_unsupported_native_fork() {
    let (svc, repo, acp_repo) = setup_fork().await;
    for backend in ["claude", "qwen"] {
        let parent = svc.create(USER_ID, fork_create_req(backend)).await.unwrap();
        if backend == "qwen" {
            acp_repo
                .update_session_id_for_user(USER_ID, &parent.id, "sid-qwen")
                .await
                .unwrap();
        }
        let m = make_message(&parent.id, "Continue the requested work", 0);
        repo.insert_message(USER_ID, &m).await.unwrap();
        let child = svc.fork(USER_ID, &parent.id, fork_req(&m.id)).await.unwrap();
        assert_eq!(child.extra["fork"]["strategy"], "handoff");
        assert_eq!(child.extra["fork"]["parent_conversation_id"], parent.id);
        assert!(child.extra["fork"].get("parent_session_id").is_none());
        let config: aionui_api_types::AcpBuildExtra = serde_json::from_value(child.extra).unwrap();
        assert!(
            config.fork.is_none(),
            "handoff lineage must not open the source session"
        );
    }
}

#[tokio::test]
async fn create_strips_client_supplied_fork_spec() {
    let (svc, _repo, _acp) = setup_fork().await;
    let workspace = ensure_named_workspace_path("aionui-fork-test-strip");
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": {
            "workspace": workspace,
            "backend": "claude",
            "fork": { "parent_conversation_id": "x", "parent_message_id": "y", "parent_session_id": "stolen" }
        }
    }))
    .unwrap();
    let created = svc.create(USER_ID, req).await.unwrap();
    assert!(
        created.extra.get("fork").is_none(),
        "a client-supplied fork spec must be stripped on the create path"
    );
}

#[tokio::test]
async fn get_projects_fork_capability_on_detail_path() {
    let (svc, _repo, _acp) = setup_fork().await;
    let claude_conv = svc.create(USER_ID, fork_create_req("claude")).await.unwrap();
    let got = svc.get(USER_ID, &claude_conv.id).await.unwrap();
    assert_eq!(
        got.fork_capability,
        Some(aionui_api_types::ForkCapabilityView { at_turn: false })
    );

    let codex_conv = svc.create(USER_ID, fork_create_req("codex")).await.unwrap();
    let got = svc.get(USER_ID, &codex_conv.id).await.unwrap();
    assert_eq!(
        got.fork_capability,
        Some(aionui_api_types::ForkCapabilityView { at_turn: true })
    );
}

#[tokio::test]
async fn get_projects_prompt_capability_on_detail_path() {
    let (svc, _repo, _acp) = setup_fork().await;

    // claude: migration 037 seeds image-only prompt capabilities.
    let claude_conv = svc.create(USER_ID, fork_create_req("claude")).await.unwrap();
    let got = svc.get(USER_ID, &claude_conv.id).await.unwrap();
    assert_eq!(
        got.prompt_capability,
        Some(aionui_api_types::PromptCapabilityView {
            image: true,
            audio: false
        })
    );

    // qwen: 003 seeds image + audio.
    let qwen_conv = svc.create(USER_ID, fork_create_req("qwen")).await.unwrap();
    let got = svc.get(USER_ID, &qwen_conv.id).await.unwrap();
    assert_eq!(
        got.prompt_capability,
        Some(aionui_api_types::PromptCapabilityView {
            image: true,
            audio: true
        })
    );
}

#[tokio::test]
async fn delete_parent_keeps_workspace_shared_with_fork() {
    let (svc, repo, acp_repo) = setup_fork().await;
    // No workspace in extra → create() auto-provisions one under
    // workspace_root/conversations/... — the deletable kind.
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "backend": "claude" }
    }))
    .unwrap();
    let parent = svc.create(USER_ID, req).await.unwrap();
    let workspace = parent
        .extra
        .get("workspace")
        .and_then(|v| v.as_str())
        .expect("auto-provisioned workspace")
        .to_owned();
    assert!(std::path::Path::new(&workspace).is_dir());

    acp_repo
        .update_session_id_for_user(USER_ID, &parent.id, "parent-sid-ws")
        .await
        .unwrap();
    let m = make_message(&parent.id, "hello", 0);
    repo.insert_message(USER_ID, &m).await.unwrap();
    let fork = svc.fork(USER_ID, &parent.id, fork_req(&m.id)).await.unwrap();
    assert_eq!(
        fork.extra.get("workspace").and_then(|v| v.as_str()),
        Some(workspace.as_str()),
        "the fork shares the parent's auto workspace"
    );

    svc.delete(USER_ID, &parent.id).await.unwrap();
    assert!(
        std::path::Path::new(&workspace).is_dir(),
        "deleting the parent must NOT remove the workspace the fork still uses"
    );

    // Deleting the fork leaves the directory too: the auto-workspace removal
    // is keyed on the `-temp-{id}` name suffix, which only the PARENT matches.
    // An orphaned directory is the accepted trade-off (safety over cleanup).
    svc.delete(USER_ID, &fork.id).await.unwrap();
    assert!(
        std::path::Path::new(&workspace).is_dir(),
        "the fork's delete never removes a workspace named after the parent"
    );
    std::fs::remove_dir_all(&workspace).ok();
}

#[tokio::test]
async fn fork_resolves_stream_msg_id_when_row_id_unknown() {
    let (svc, repo, acp_repo) = setup_fork().await;
    let parent = svc.create(USER_ID, fork_create_req("claude")).await.unwrap();
    acp_repo
        .update_session_id_for_user(USER_ID, &parent.id, "parent-sid-msgid")
        .await
        .unwrap();
    // make_message mints msg_id ("client_...") distinct from the row id — the
    // shape a live-streamed frontend message sends (its local `id` is never
    // persisted, only the stream msg_id matches anything in the DB).
    let m = make_message(&parent.id, "hello", 0);
    repo.insert_message(USER_ID, &m).await.unwrap();

    let fork = svc
        .fork(USER_ID, &parent.id, fork_req(m.msg_id.as_deref().unwrap()))
        .await
        .expect("msg_id fallback resolves the fork point");
    assert_eq!(
        fork.extra["fork"]["parent_message_id"], m.id,
        "resolved to the real row"
    );
}

// ── Conversation fork: builtin aionrs agent (no acp_session row) ────

fn aionrs_create_req() -> CreateConversationRequest {
    let workspace = ensure_named_workspace_path("aionui-fork-test-aionrs");
    serde_json::from_value(json!({
        "type": "aionrs",
        "extra": { "workspace": workspace, "backend": "aionrs" }
    }))
    .unwrap()
}

#[tokio::test]
async fn fork_head_aionrs_uses_conversation_id_anchor_without_acp_row() {
    let (svc, repo, acp_repo) = setup_fork().await;
    let parent = svc.create(USER_ID, aionrs_create_req()).await.unwrap();
    // aionrs conversations own no acp_session row (create() only makes one
    // for ACP/antigravity) — the whole point of this path. The agent
    // identity resolves through the builtin binding ladder (agent_type
    // "aionrs" -> seed row), no assistant snapshot required.
    assert!(acp_repo.get_for_user(USER_ID, &parent.id).await.unwrap().is_none());
    let m1 = make_message(&parent.id, "hello", 0);
    let m2 = make_message(&parent.id, "world", 10);
    repo.insert_message(USER_ID, &m1).await.unwrap();
    repo.insert_message(USER_ID, &m2).await.unwrap();

    let fork = svc.fork(USER_ID, &parent.id, fork_req(&m2.id)).await.unwrap();
    assert_ne!(fork.id, parent.id);
    assert_eq!(
        fork.fork_capability,
        Some(aionui_api_types::ForkCapabilityView { at_turn: true }),
        "aionrs declares an at-turn fork capability (migration 038)"
    );
    let spec = fork.extra.get("fork").expect("fork lineage in extra");
    assert_eq!(spec["parent_conversation_id"], parent.id);
    assert_eq!(spec["parent_message_id"], m2.id);
    assert_eq!(
        spec["parent_session_id"], parent.id,
        "aionrs sessions are keyed by conversation id, so the parent conversation id is the session anchor"
    );
    assert!(spec.get("last_turn_id").is_none(), "aionrs forks only at HEAD");

    // Copied history.
    let page = repo
        .list_messages_page(
            USER_ID,
            &fork.id,
            &aionui_db::MessagePageParams {
                limit: 50,
                direction: aionui_db::MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);

    // The fork, like every aionrs conversation, owns no acp_session row.
    assert!(acp_repo.get_for_user(USER_ID, &fork.id).await.unwrap().is_none());
}

#[tokio::test]
async fn fork_mid_history_without_anchor_is_refused_for_aionrs() {
    let (svc, repo, _acp_repo) = setup_fork().await;
    let parent = svc.create(USER_ID, aionrs_create_req()).await.unwrap();
    // Rows without backend_turn_id: written before turn stamping existed.
    let m1 = make_message(&parent.id, "one", 0);
    let m2 = make_message(&parent.id, "two", 10);
    repo.insert_message(USER_ID, &m1).await.unwrap();
    repo.insert_message(USER_ID, &m2).await.unwrap();

    let err = svc.fork(USER_ID, &parent.id, fork_req(&m1.id)).await.unwrap_err();
    assert!(
        matches!(&err, ConversationError::Unprocessable { reason } if reason.starts_with("FORK_POINT_UNSUPPORTED")),
        "unanchored aionrs mid-history fork must be refused, not degraded to HEAD, got {err:?}"
    );
}

#[tokio::test]
async fn fork_mid_history_resolves_aionrs_turn_anchor() {
    let (svc, repo, _acp_repo) = setup_fork().await;
    let parent = svc.create(USER_ID, aionrs_create_req()).await.unwrap();
    // Rows stamped by the aionrs turn wiring (manager emits BackendTurnBound
    // with the conversation-layer turn id).
    let mut m1 = make_message(&parent.id, "one", 0);
    m1.backend_turn_id = Some("turn_aion1".into());
    let m2 = make_message(&parent.id, "two", 10);
    repo.insert_message(USER_ID, &m1).await.unwrap();
    repo.insert_message(USER_ID, &m2).await.unwrap();

    let fork = svc.fork(USER_ID, &parent.id, fork_req(&m1.id)).await.unwrap();
    let spec = fork.extra.get("fork").expect("fork lineage in extra");
    assert_eq!(
        spec["last_turn_id"], "turn_aion1",
        "mid-history fork snapshots the resolved aionrs turn anchor"
    );
    assert_eq!(spec["parent_session_id"], parent.id);
}

#[tokio::test]
async fn get_projects_fork_capability_for_aionrs_on_detail_path() {
    let (svc, _repo, _acp_repo) = setup_fork().await;
    let conv = svc.create(USER_ID, aionrs_create_req()).await.unwrap();

    let detail = svc.get(USER_ID, &conv.id).await.unwrap();
    assert_eq!(
        detail.fork_capability,
        Some(aionui_api_types::ForkCapabilityView { at_turn: true }),
        "detail path projects the aionrs fork capability via the builtin binding ladder"
    );
}

#[tokio::test]
async fn fork_operation_is_durable_concurrent_and_rejects_conflicting_reuse() {
    let (svc, repo, acp) = setup_fork().await;
    let parent = svc.create(USER_ID, fork_create_req("codex")).await.unwrap();
    acp.update_session_id_for_user(USER_ID, &parent.id, "source-session")
        .await
        .unwrap();
    let m = make_message(&parent.id, "ship this feature", 0);
    repo.insert_message(USER_ID, &m).await.unwrap();
    let request = || serde_json::from_value(json!({"message_id":m.id,"operation_id":"retry-123"})).unwrap();
    let (a, b) = tokio::join!(
        svc.fork(USER_ID, &parent.id, request()),
        svc.fork(USER_ID, &parent.id, request())
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(a.id, b.id);
    let fresh_service = svc.clone();
    let repeated = fresh_service.fork(USER_ID, &parent.id, request()).await.unwrap();
    assert_eq!(repeated.id, a.id);
    let messages = repo
        .list_messages_page(
            USER_ID,
            &a.id,
            &aionui_db::MessagePageParams {
                limit: 50,
                direction: aionui_db::MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    assert_eq!(messages.items.len(), 1, "retry does not copy the history twice");
    let conflict =
        serde_json::from_value(json!({"message_id":m.id,"operation_id":"retry-123","name":"different"})).unwrap();
    let error = svc.fork(USER_ID, &parent.id, conflict).await.unwrap_err();
    assert!(matches!(error, ConversationError::Busy { reason } if reason.starts_with("FORK_OPERATION_CONFLICT")));
}

#[tokio::test]
async fn fork_keeps_source_turn_and_excludes_runtime_queues_and_capabilities() {
    let (svc, repo, acp) = setup_fork().await;
    let runtime = Arc::new(aionui_conversation::runtime_state::ConversationRuntimeStateService::default());
    let svc = svc.with_runtime_state(runtime.clone());
    let mut req = fork_create_req("codex");
    req.extra["session_id"] = json!("forbidden-old-session");
    req.extra["runtime"] = json!({"state":"running"});
    req.extra["capabilities"] = json!({"old":true});
    req.extra["commandQueue"] = json!([{"content":"source queue must survive"}]);
    let parent = svc.create(USER_ID, req).await.unwrap();
    acp.update_session_id_for_user(USER_ID, &parent.id, "source-sid")
        .await
        .unwrap();
    let m = make_message(&parent.id, "working", 0);
    let mut m = m;
    m.backend_turn_id = Some("backend-active-turn".into());
    repo.insert_message(USER_ID, &m).await.unwrap();
    let mut next = make_message(&parent.id, "still working after the click", 10);
    next.backend_turn_id = Some("backend-active-turn".into());
    repo.insert_message(USER_ID, &next).await.unwrap();
    let _source_claim = runtime.try_claim_turn(&parent.id, "source-active-turn").unwrap();
    let child = svc.fork(USER_ID, &parent.id, fork_req(&m.id)).await.unwrap();
    assert!(
        child.extra["fork"].get("last_turn_id").is_none(),
        "a running turn must fork live HEAD rather than pass an in-progress lastTurnId"
    );
    assert_eq!(
        runtime.active_turn_id_for(&parent.id).as_deref(),
        Some("source-active-turn")
    );
    for key in ["session_id", "runtime", "capabilities", "commandQueue"] {
        assert!(child.extra.get(key).is_none(), "{key} leaked");
    }
    let source = svc.get(USER_ID, &parent.id).await.unwrap();
    assert_eq!(source.extra["commandQueue"][0]["content"], "source queue must survive");
    let bound = acp.get_for_user(USER_ID, &child.id).await.unwrap().unwrap();
    assert!(bound.session_id.is_none());
    let state = acp
        .load_runtime_state_for_user(USER_ID, &child.id)
        .await
        .unwrap()
        .unwrap();
    assert!(state.current_model_id.is_none());
    assert!(state.current_mode_id.is_none());
}

#[tokio::test]
async fn fork_cross_agent_builds_bounded_handoff_and_fresh_target_binding() {
    let (svc, repo, acp) = setup_fork().await;
    let parent = svc.create(USER_ID, fork_create_req("codex")).await.unwrap();
    let target = svc.create(USER_ID, fork_create_req("claude")).await.unwrap();
    let target_id = acp.get_for_user(USER_ID, &target.id).await.unwrap().unwrap().agent_id;
    acp.update_session_id_for_user(USER_ID, &parent.id, "source-codex")
        .await
        .unwrap();
    let goal = make_message(&parent.id, "Implement the login screen", 0);
    repo.insert_message(USER_ID, &goal).await.unwrap();
    let mut plan = make_message(&parent.id, "", 10);
    plan.r#type = "plan".into();
    plan.position = Some("left".into());
    plan.content = json!({"entries":[{"content":"finish login tests","status":"pending"}]}).to_string();
    repo.insert_message(USER_ID, &plan).await.unwrap();
    let mut change = make_message(&parent.id, "", 20);
    change.r#type = "acp_tool_call".into();
    change.content = json!({"name":"apply_patch","file_path":"src/Login.tsx","status":"completed"}).to_string();
    repo.insert_message(USER_ID, &change).await.unwrap();
    let mut result = make_message(&parent.id, "", 30);
    result.position = Some("left".into());
    result.content = json!({"content":"npm test: 3 tests passed. Next: finish the pending login tests."}).to_string();
    repo.insert_message(USER_ID, &result).await.unwrap();
    let req = serde_json::from_value(
        json!({"message_id":result.id,"operation_id":"cross-agent","target_agent_id":target_id}),
    )
    .unwrap();
    let child = svc.fork(USER_ID, &parent.id, req).await.unwrap();
    assert_eq!(child.extra["backend"], "claude");
    assert_eq!(child.extra["fork"]["strategy"], "handoff");
    let config: aionui_api_types::AcpBuildExtra = serde_json::from_value(child.extra.clone()).unwrap();
    assert!(config.fork.is_none());
    let summary = child.extra["fork_operation"]["summary"].as_str().unwrap();
    for expected in [
        "Goal",
        "Latest plan",
        "Code changes",
        "Test results",
        "Next steps",
        "login",
        "src/Login.tsx",
        "3 tests passed",
    ] {
        assert!(summary.contains(expected), "missing {expected}: {summary}");
    }
    assert!(summary.chars().count() < 15_000);
    let messages = repo
        .list_messages_page(
            USER_ID,
            &child.id,
            &aionui_db::MessagePageParams {
                limit: 50,
                direction: aionui_db::MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    assert_eq!(messages.items.len(), 1, "summary only, not the source transcript");
    let binding = acp.get_for_user(USER_ID, &child.id).await.unwrap().unwrap();
    assert_eq!(binding.agent_id, target_id);
    assert!(binding.session_id.is_none());
}

#[tokio::test]
async fn fork_continuation_retries_do_not_duplicate_the_task() {
    let (svc, repo, _) = setup_fork().await;
    let parent = svc.create(USER_ID, fork_create_req("claude")).await.unwrap();
    let m = make_message(&parent.id, "continue work", 0);
    repo.insert_message(USER_ID, &m).await.unwrap();
    let req = || {
        serde_json::from_value(json!({"message_id":m.id,"operation_id":"continue-once","continue_task":true})).unwrap()
    };
    let child = svc.fork(USER_ID, &parent.id, req()).await.unwrap();
    let retried = svc.fork(USER_ID, &parent.id, req()).await.unwrap();
    assert_eq!(child.id, retried.id);
    let messages = repo
        .list_messages_page(
            USER_ID,
            &child.id,
            &aionui_db::MessagePageParams {
                limit: 50,
                direction: aionui_db::MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        messages
            .items
            .iter()
            .filter(|m| m.position.as_deref() == Some("right"))
            .count(),
        1
    );
    assert!(!repo.claim_fork_continuation(USER_ID, &child.id).await.unwrap());
    assert!(!repo.claim_fork_continuation("another-user", &child.id).await.unwrap());
}

#[tokio::test]
async fn fork_rejects_invalid_operations_missing_targets_and_foreign_sources() {
    let (svc, repo, _) = setup_fork().await;
    let parent = svc.create(USER_ID, fork_create_req("claude")).await.unwrap();
    let m = make_message(&parent.id, "hello", 0);
    repo.insert_message(USER_ID, &m).await.unwrap();
    let request = serde_json::from_value(json!({"message_id":m.id,"operation_id":"bad key"})).unwrap();
    assert!(
        matches!(svc.fork(USER_ID, &parent.id, request).await.unwrap_err(), ConversationError::BadRequest { reason } if reason.starts_with("FORK_INVALID_OPERATION"))
    );
    let request = serde_json::from_value(json!({"message_id":m.id,"target_agent_id":"unknown-agent"})).unwrap();
    assert!(
        matches!(svc.fork(USER_ID, &parent.id, request).await.unwrap_err(), ConversationError::BadRequest { reason } if reason.starts_with("FORK_TARGET_UNAVAILABLE"))
    );
    assert!(matches!(
        svc.fork("another-user", &parent.id, fork_req(&m.id)).await.unwrap_err(),
        ConversationError::NotFound { .. }
    ));
}
