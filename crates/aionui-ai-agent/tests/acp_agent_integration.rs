//! Integration tests for AcpAgentManager.
//!
//! Lifecycle regressions use a standalone JSON-RPC responder. The older shell
//! mock cases at the bottom remain ignored because they predate the ACP
//! initialize handshake.
//!
//! Tests are serialized via `SERIAL_LOCK` to avoid OS-level resource
//! contention from parallel subprocess spawning (pipes, I/O scheduling).

// Pre-existing: serial() MutexGuard held across await points is intentional —
// it serializes test execution. Useless .into() is a pre-existing nit.
#![allow(clippy::await_holding_lock, clippy::useless_conversion)]

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use aionui_ai_agent::factory::acp_assembler::{WorkspaceInfo, assemble_acp_params};
use aionui_ai_agent::manager::acp::AcpAgentManager;
use aionui_ai_agent::registry::AgentRegistry;
use aionui_ai_agent::{AgentInstance, AgentStreamEvent, IAgentTask};
use aionui_common::ConversationStatus;
use aionui_db::{SqliteAgentMetadataRepository, init_database_memory};
use tokio::sync::broadcast;

/// Timeout for receiving events from the relay.
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

/// Serialize integration tests to avoid OS-level resource contention
/// from parallel subprocess spawning (pipes, I/O scheduling).
static SERIAL_LOCK: Mutex<()> = Mutex::new(());

/// Acquire the serial lock (panics on poison).
fn serial() -> MutexGuard<'static, ()> {
    SERIAL_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Create an AcpAgentManager wrapping a mock shell script.
///
/// Returns the Arc-wrapped manager and a pre-subscribed event receiver
/// (subscribed BEFORE the relay starts, so no events are missed).
async fn make_mock_agent(script: &str, backend: &str) -> (Arc<AcpAgentManager>, broadcast::Receiver<AgentStreamEvent>) {
    let temp_dir = std::env::temp_dir();
    let script_path = temp_dir.join(format!(
        "mock_acp_{}_{}.sh",
        std::process::id(),
        aionui_common::now_ms()
    ));
    std::fs::write(&script_path, format!("#!/bin/sh\n{script}")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    make_agent_with_command(
        aionui_common::CommandSpec {
            command: script_path.into(),
            args: vec![],
            env: vec![],
            cwd: None,
        },
        backend,
    )
    .await
}

async fn make_agent_with_command(
    command_spec: aionui_common::CommandSpec,
    backend: &str,
) -> (Arc<AcpAgentManager>, broadcast::Receiver<AgentStreamEvent>) {
    let config = aionui_ai_agent::AcpBuildExtra {
        agent_id: None,
        backend: Some(backend.to_owned()),
        cli_path: Some(command_spec.command.to_string_lossy().into_owned()),
        agent_name: None,
        custom_agent_id: None,
        preset_context: None,
        skills: vec![],
        preset_assistant_id: None,
        session_mode: None,
        current_model_id: None,
        thought_level: None,
        cron_job_id: None,
        team_mcp_stdio_config: None,
        mcp_server_ids: None,
        session_mcp_servers: vec![],
        user_id: None,
        fork: None,
    };

    let tmp_skills = tempfile::TempDir::new().unwrap();
    let skill_paths = std::sync::Arc::new(aionui_extension::resolve_skill_paths(
        tmp_skills.path(),
        tmp_skills.path(),
    ));
    let skill_manager = aionui_ai_agent::AcpSkillManager::new(skill_paths);

    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let registry = AgentRegistry::new(repo);
    registry.hydrate().await.unwrap();

    let metadata = registry
        .find_builtin_by_backend(backend)
        .await
        .expect("seeded backend row must exist");
    let catalog_tx = registry.catalog_sender();

    let params = Arc::new(
        assemble_acp_params(
            "test-conv-1".into(),
            "user-acp-test".into(),
            WorkspaceInfo {
                path: "/tmp".into(),
                is_custom: true,
            },
            metadata,
            command_spec,
            config,
            Vec::new(),
            None,
            std::env::temp_dir(),
            false,
        )
        .await,
    );

    let (manager, _, _) = AcpAgentManager::build(params, skill_manager, &catalog_tx)
        .await
        .expect("Failed to spawn mock ACP agent");

    let arc = Arc::new(manager);

    // Subscribe to typed events BEFORE starting handler to capture all events
    let rx = arc.subscribe();
    arc.start_permission_handler();

    (arc, rx)
}

/// Wait until a specific event type is received, returning all collected events.
async fn wait_for_event(
    rx: &mut broadcast::Receiver<AgentStreamEvent>,
    predicate: impl Fn(&AgentStreamEvent) -> bool,
) -> Vec<AgentStreamEvent> {
    let mut events = Vec::new();
    loop {
        match tokio::time::timeout(EVENT_TIMEOUT, rx.recv()).await {
            Ok(Ok(event)) => {
                let matched = predicate(&event);
                events.push(event);
                if matched {
                    return events;
                }
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                panic!(
                    "Event channel closed before target event. Received: {:?}",
                    events.iter().map(event_type_name).collect::<Vec<_>>()
                )
            }
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                eprintln!("Warning: receiver lagged by {n} events");
                continue;
            }
            Err(_) => panic!(
                "Timed out waiting for target event. Received: {:?}",
                events.iter().map(event_type_name).collect::<Vec<_>>()
            ),
        }
    }
}

/// Get a short name for the event type (for debug output).
fn event_type_name(event: &AgentStreamEvent) -> &'static str {
    match event {
        AgentStreamEvent::Start(_) => "Start",
        AgentStreamEvent::Text(_) => "Text",
        AgentStreamEvent::Tips(_) => "Tips",
        AgentStreamEvent::ToolCall(_) => "ToolCall",
        AgentStreamEvent::ToolGroup(_) => "ToolGroup",
        AgentStreamEvent::AgentStatus(_) => "AgentStatus",
        AgentStreamEvent::Thinking(_) => "Thinking",
        AgentStreamEvent::Plan(_) => "Plan",
        AgentStreamEvent::Permission(_) => "Permission",
        AgentStreamEvent::AcpPermission(_) => "AcpPermission",
        AgentStreamEvent::Ask(_) => "Ask",
        AgentStreamEvent::AcpToolCall(_) => "AcpToolCall",
        AgentStreamEvent::AvailableCommands(_) => "AvailableCommands",
        AgentStreamEvent::SkillSuggest(_) => "SkillSuggest",
        AgentStreamEvent::CronTrigger(_) => "CronTrigger",
        AgentStreamEvent::AcpModelInfo(_) => "AcpModelInfo",
        AgentStreamEvent::AcpModeInfo(_) => "AcpModeInfo",
        AgentStreamEvent::AcpConfigOption(_) => "AcpConfigOption",
        AgentStreamEvent::AcpSessionInfo(_) => "AcpSessionInfo",
        AgentStreamEvent::AcpContextUsage(_) => "AcpContextUsage",
        AgentStreamEvent::AcpTerminalOutput(_) => "AcpTerminalOutput",
        AgentStreamEvent::AcpPromptHookWarning(_) => "AcpPromptHookWarning",
        AgentStreamEvent::Finish(_) => "Finish",
        AgentStreamEvent::Error(_) => "Error",
        AgentStreamEvent::System(_) => "System",
        AgentStreamEvent::RequestTrace(_) => "RequestTrace",
        AgentStreamEvent::SlashCommandsUpdated(_) => "SlashCommandsUpdated",
        AgentStreamEvent::SessionAssigned(_) => "SessionAssigned",
        AgentStreamEvent::SegmentBreak => "SegmentBreak",
        AgentStreamEvent::BackendTurnBound(_) => "BackendTurnBound",
        AgentStreamEvent::WorkflowProgress(_) => "WorkflowProgress",
        AgentStreamEvent::AcpDialectSignal(_) => "AcpDialectSignal",
        AgentStreamEvent::MessageLifecycle(_) => "MessageLifecycle",
    }
}

#[test]
fn acp_build_extra_populates_skills_from_extra_json() {
    let json = serde_json::json!({
        "backend": "claude",
        "skills": ["cron", "pdf"],
    });
    let extra: aionui_ai_agent::AcpBuildExtra = serde_json::from_value(json).unwrap();
    assert_eq!(extra.skills, vec!["cron".to_owned(), "pdf".to_owned()]);
}

fn test_send_data(content: &str, msg_id: &str) -> aionui_ai_agent::types::SendMessageData {
    aionui_ai_agent::types::SendMessageData {
        content: content.to_owned(),
        msg_id: msg_id.to_owned(),
        turn_id: None,
        files: vec![],
        inject_skills: vec![],
    }
}

async fn make_test_agent(
    scenario: &str,
) -> (
    Arc<AcpAgentManager>,
    broadcast::Receiver<AgentStreamEvent>,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let nonce = format!("{}_{}", std::process::id(), aionui_common::now_ms());
    let trace = std::env::temp_dir().join(format!("acp-test-trace-{nonce}"));
    let release = std::env::temp_dir().join(format!("acp-test-release-{nonce}"));
    let command_spec = aionui_common::CommandSpec {
        command: "node".into(),
        args: vec![
            "-e".into(),
            TEST_AGENT_SCRIPT.into(),
            scenario.into(),
            trace.to_string_lossy().into_owned(),
            release.to_string_lossy().into_owned(),
        ],
        env: vec![],
        cwd: None,
    };
    let (agent, rx) = make_agent_with_command(command_spec, "kimi").await;
    (agent, rx, trace, release)
}

async fn wait_for_trace(trace: &std::path::Path, expected: &str) -> String {
    tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            let text = std::fs::read_to_string(trace).unwrap_or_default();
            if text.contains(expected) {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {expected:?} in {}", trace.display()))
}

const TEST_AGENT_SCRIPT: &str = r#"
const fs = require('node:fs');
const readline = require('node:readline');
const [scenario, trace, release] = process.argv.slice(1);
const pending = new Map();
let cancelled = false;
let active = 0;
let promptSeq = 0;
let requestSeq = 0;
const send = (message) => process.stdout.write(JSON.stringify(message) + '\n');
const append = (line) => fs.appendFileSync(trace, line + '\n');
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const respond = (id, result) => send({ jsonrpc: '2.0', id, result });
const call = (method, params) => new Promise((resolve, reject) => {
  const id = `client-${++requestSeq}`;
  pending.set(id, { resolve, reject });
  send({ jsonrpc: '2.0', id, method, params });
});
async function prompt(request) {
  const sequence = ++promptSeq;
  append(`start:${sequence}:active:${++active}`);
  if (scenario === 'serialize' && sequence === 1) {
    while (!fs.existsSync(release) && !cancelled) await sleep(10);
  }
  if (scenario === 'terminal') {
    const command = process.platform === 'win32' ? 'ping -n 30 127.0.0.1' : 'sleep 30';
    const terminal = await call('terminal/create', { sessionId: request.params.sessionId, command });
    fs.writeFileSync(trace, terminal.terminalId);
  }
  --active;
  if (scenario !== 'terminal') append(`end:${sequence}`);
  respond(request.id, { stopReason: cancelled ? 'cancelled' : 'end_turn' });
}
async function main() {
  for await (const line of readline.createInterface({ input: process.stdin })) {
    const message = JSON.parse(line);
    if (message.id !== undefined && pending.has(message.id)) {
      const handler = pending.get(message.id);
      pending.delete(message.id);
      if (message.error) handler.reject(new Error(JSON.stringify(message.error)));
      else handler.resolve(message.result);
    } else if (message.method === 'initialize') {
      respond(message.id, {
        protocolVersion: message.params.protocolVersion,
        agentCapabilities: {},
        authMethods: [{ id: 'test-login', name: 'Test login', description: 'Test login' }],
      });
    } else if (message.method === 'session/new') {
      respond(message.id, { sessionId: 'test-session' });
    } else if (message.method === 'session/prompt') {
      void prompt(message).catch((error) => send({
        jsonrpc: '2.0',
        id: message.id,
        error: { code: -32603, message: String(error) },
      }));
    } else if (message.method === 'session/cancel') {
      cancelled = true;
    }
  }
}
void main();
"#;

#[tokio::test]
async fn acp_prompts_are_serialized_including_terminal_events() {
    let _guard = serial();
    let (agent, _rx, trace, release) = make_test_agent("serialize").await;
    let first_agent = Arc::clone(&agent);
    let first = tokio::spawn(async move { first_agent.send_message(test_send_data("first", "first")).await });
    wait_for_trace(&trace, "start:1").await;

    let second_agent = Arc::clone(&agent);
    let second = tokio::spawn(async move { second_agent.send_message(test_send_data("second", "second")).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let overlapped = std::fs::read_to_string(&trace).unwrap_or_default().contains("active:2");
    std::fs::write(&release, b"release").unwrap();
    let first_result = tokio::time::timeout(EVENT_TIMEOUT, first).await.unwrap();
    let second_result = tokio::time::timeout(EVENT_TIMEOUT, second).await.unwrap();
    let final_trace = wait_for_trace(&trace, "end:2").await;
    agent.kill_and_wait(None).await;
    let _ = std::fs::remove_file(&trace);
    let _ = std::fs::remove_file(&release);

    assert!(
        !overlapped,
        "two ACP prompts entered the backend together: {final_trace}"
    );
    assert!(matches!(first_result, Ok(Ok(()))));
    assert!(matches!(second_result, Ok(Ok(()))));
    assert!(final_trace.find("end:1").unwrap() < final_trace.find("start:2").unwrap());
    assert_eq!(agent.status(), Some(ConversationStatus::Finished));
}

#[tokio::test]
async fn acp_cancel_does_not_wait_for_the_prompt_lock() {
    let _guard = serial();
    let (agent, _rx, trace, release) = make_test_agent("serialize").await;
    let sending_agent = Arc::clone(&agent);
    let sending = tokio::spawn(async move { sending_agent.send_message(test_send_data("long", "long")).await });
    wait_for_trace(&trace, "start:1").await;

    tokio::time::timeout(EVENT_TIMEOUT, agent.cancel())
        .await
        .unwrap()
        .unwrap();
    let result = tokio::time::timeout(EVENT_TIMEOUT, sending).await.unwrap();
    agent.kill_and_wait(None).await;
    let _ = std::fs::remove_file(&trace);
    let _ = std::fs::remove_file(&release);
    assert!(matches!(result, Ok(Ok(()))));
}

#[tokio::test]
async fn acp_live_terminal_prevents_idle_cleanup_and_auth_hint() {
    let _guard = serial();
    let (agent, mut rx, trace, release) = make_test_agent("terminal").await;
    tokio::time::timeout(EVENT_TIMEOUT, agent.send_message(test_send_data("long", "long")))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(agent.live_background_tasks(), 1);
    let terminal_id = std::fs::read_to_string(&trace).unwrap();
    let mut tips = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentStreamEvent::Tips(data) = event {
            tips.push(data.code);
        }
    }
    assert!(!tips.contains(&Some("ACP_EMPTY_TURN_NEEDS_AUTH".into())));
    assert!(!tips.contains(&Some("ACP_EMPTY_TURN".into())));

    assert!(agent.kill_client_terminal(&terminal_id).await);
    assert_eq!(agent.live_background_tasks(), 0);
    agent.kill_and_wait(None).await;
    let _ = std::fs::remove_file(&trace);
    let _ = std::fs::remove_file(&release);
}

// -- Tests --
// All tests below are #[ignore] because make_mock_agent() spawns shell scripts
// that cannot respond to the SDK's JSON-RPC `initialize` handshake.

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_type_is_acp() {
    let _guard = serial();
    let (agent, _rx) = make_mock_agent(r#"echo '{"type":"finish","data":{}}'"#, "claude").await;

    assert_eq!(agent.agent_type(), aionui_common::AgentType::Acp);
    assert_eq!(agent.conversation_id(), "test-conv-1");
    assert_eq!(agent.workspace(), "/tmp");
    assert_eq!(agent.backend(), Some("claude"));
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_receives_stream_events() {
    let _guard = serial();
    let (_agent, mut rx) = make_mock_agent(
        r#"echo '{"type":"start","data":{"session_id":"sess-1"}}' && echo '{"type":"text","data":{"content":"Hello"}}' && echo '{"type":"finish","data":{"session_id":"sess-1"}}'"#,
        "claude",
    )
    .await;

    // Wait for finish event, collecting all events along the way
    let events = wait_for_event(&mut rx, |e| matches!(e, AgentStreamEvent::Finish(_))).await;

    assert!(events.len() >= 2, "Expected at least 2 events, got {}", events.len());

    let has_start = events.iter().any(|e| matches!(e, AgentStreamEvent::Start(_)));
    let has_text = events.iter().any(|e| matches!(e, AgentStreamEvent::Text(_)));

    assert!(has_start, "Should have received Start event");
    assert!(has_text, "Should have received Text event");
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_session_id_captured_from_start() {
    let _guard = serial();
    let (agent, mut rx) = make_mock_agent(
        r#"echo '{"type":"start","data":{"session_id":"sess-abc"}}' && sleep 1"#,
        "claude",
    )
    .await;

    wait_for_event(&mut rx, |e| matches!(e, AgentStreamEvent::Start(_))).await;

    let session_id = agent.session_id().await;
    assert_eq!(session_id, Some("sess-abc".into()));

    agent.kill(None).unwrap();
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_status_transitions() {
    let _guard = serial();
    let (agent, mut rx) = make_mock_agent(
        r#"sleep 0.1 && echo '{"type":"start","data":{}}' && sleep 0.3 && echo '{"type":"finish","data":{}}'"#,
        "claude",
    )
    .await;

    // Initial status: None
    assert_eq!(agent.status(), None);

    // Wait for Start event
    wait_for_event(&mut rx, |e| matches!(e, AgentStreamEvent::Start(_))).await;
    assert_eq!(agent.status(), Some(ConversationStatus::Running));

    // Wait for Finish event
    wait_for_event(&mut rx, |e| matches!(e, AgentStreamEvent::Finish(_))).await;
    assert_eq!(agent.status(), Some(ConversationStatus::Finished));
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_error_event_sets_finished() {
    let _guard = serial();
    let (agent, mut rx) = make_mock_agent(
        r#"echo '{"type":"start","data":{}}' && sleep 0.1 && echo '{"type":"error","data":{"message":"timeout"}}'"#,
        "claude",
    )
    .await;

    wait_for_event(&mut rx, |e| matches!(e, AgentStreamEvent::Error(_))).await;
    assert_eq!(agent.status(), Some(ConversationStatus::Finished));
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_model_info_captured() {
    let _guard = serial();
    let (agent, mut rx) = make_mock_agent(
        r#"echo '{"type":"acp_model_info","data":{"current_model_id":"claude-sonnet-4","current_model_label":"Claude Sonnet 4","available_models":[{"id":"claude-sonnet-4","label":"Claude Sonnet 4"},{"id":"claude-opus-4","label":"Claude Opus 4"}],"can_switch":true,"source":"models","source_detail":"acp-models"}}' && sleep 0.5"#,
        "claude",
    )
    .await;

    wait_for_event(&mut rx, |e| matches!(e, AgentStreamEvent::AcpModelInfo(_))).await;

    // Route through the public `AgentInstance` API rather than reaching
    // into the private `AcpAgentManager::model()`: the ai-agent crate only
    // exposes `AgentInstance` to downstream callers, so tests should
    // exercise the same surface.
    let instance = AgentInstance::Acp(agent.clone());
    let resp = instance.get_model().await.expect("get_model should succeed");
    let info = resp.model_info.expect("Model info should be captured");
    assert_eq!(info.current_model_id.as_deref(), Some("claude-sonnet-4"));
    assert_eq!(info.available_models.len(), 2);
    assert_eq!(info.available_models[0].label, "Claude Sonnet 4");

    agent.kill(None).unwrap();
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_kill_terminates_process() {
    let _guard = serial();
    let (agent, _rx) = make_mock_agent(r#"trap '' TERM; while true; do sleep 1; done"#, "claude").await;

    assert!(agent.last_activity_at() > 0);

    agent.kill(Some(aionui_common::AgentKillReason::IdleTimeout)).unwrap();

    tokio::time::sleep(Duration::from_millis(1000)).await;
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_last_activity_updates() {
    let _guard = serial();
    let (agent, _rx) = make_mock_agent(r#"sleep 10"#, "claude").await;

    let initial = agent.last_activity_at();
    assert!(initial > 0);

    let now = aionui_common::now_ms();
    assert!(now - initial < 5000, "Last activity should be recent");

    agent.kill(None).unwrap();
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_text_content_received() {
    let _guard = serial();
    let (_agent, mut rx) = make_mock_agent(
        r#"echo '{"type":"text","data":{"content":"Hello from ACP"}}'"#,
        "claude",
    )
    .await;

    match tokio::time::timeout(EVENT_TIMEOUT, rx.recv()).await {
        Ok(Ok(AgentStreamEvent::Text(data))) => {
            assert_eq!(data.content, "Hello from ACP");
        }
        other => panic!("Expected Text event, got {:?}", other),
    }
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_agent_status_event_captures_session() {
    let _guard = serial();
    let (agent, mut rx) = make_mock_agent(
        r#"echo '{"type":"agent_status","data":{"backend":"claude","status":"running","session_id":"sess-xyz"}}' && sleep 1"#,
        "claude",
    )
    .await;

    wait_for_event(&mut rx, |e| matches!(e, AgentStreamEvent::AgentStatus(_))).await;

    let session = agent.session_id().await;
    assert_eq!(session, Some("sess-xyz".into()));

    agent.kill(None).unwrap();
}

#[tokio::test]
#[ignore = "requires JSON-RPC mock agent"]
async fn acp_agent_multiple_event_types() {
    let _guard = serial();
    let (_agent, mut rx) = make_mock_agent(
        r#"echo '{"type":"start","data":{"session_id":"sess-multi"}}' && echo '{"type":"thinking","data":{"content":"Analyzing...","subject":"code","duration":100,"status":"in_progress"}}' && echo '{"type":"text","data":{"content":"Result"}}' && echo '{"type":"finish","data":{"session_id":"sess-multi"}}'"#,
        "claude",
    )
    .await;

    let events = wait_for_event(&mut rx, |e| matches!(e, AgentStreamEvent::Finish(_))).await;

    assert!(events.len() >= 4, "Expected 4+ events, got {}", events.len());

    assert!(matches!(&events[0], AgentStreamEvent::Start(d) if d.session_id == Some("sess-multi".into())));
    assert!(matches!(&events[1], AgentStreamEvent::Thinking(d) if d.content == "Analyzing..."));
    assert!(matches!(&events[2], AgentStreamEvent::Text(d) if d.content == "Result"));
    assert!(matches!(&events[3], AgentStreamEvent::Finish(d) if d.session_id == Some("sess-multi".into())));
}
