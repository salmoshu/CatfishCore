//! Opt-in native account-switch test. Restores history but sends no model request.
//! Requires CODEX_RESUME_TEST_THREAD and CODEX_RESUME_TEST_BINARY plus an existing
//! account-manager profile selected in ~/.config/ai-usage-monitor/codex-accounts.json.

use std::sync::Arc;

use aionui_common::EnvVar;
use aionui_session::{
    BackendConnection, BackendError, CodexConnection, Command, CommandMeta, ContentBlock, SessionConfig, SessionSpec,
};
use futures_util::StreamExt;

#[tokio::test]
#[ignore = "requires local Codex history and two configured ChatGPT profiles; sends no model request"]
async fn switches_accounts_and_restores_native_tool_history_without_a_fresh_thread() {
    let thread_id = std::env::var("CODEX_RESUME_TEST_THREAD").expect("CODEX_RESUME_TEST_THREAD");
    let binary = std::env::var("CODEX_RESUME_TEST_BINARY").expect("CODEX_RESUME_TEST_BINARY");
    let user_home =
        std::path::PathBuf::from(std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap());
    let registry_path = user_home.join(".config/ai-usage-monitor/codex-accounts.json");
    let mut accounts: serde_json::Value = serde_json::from_slice(&std::fs::read(registry_path).unwrap()).unwrap();
    let target = accounts["active"].as_str().expect("selected account").to_owned();
    assert_ne!(
        target, "default",
        "select the target account before running this opt-in test"
    );
    let directory = tempfile::tempdir().unwrap();
    let test_registry = directory.path().join("accounts.json");
    accounts["active"] = serde_json::json!("default");
    std::fs::write(&test_registry, accounts.to_string()).unwrap();
    let spawner = Arc::new(aionui_process::RealSpawner::new(
        Arc::new(aionui_process::FileRegistryStore::new(directory.path())),
        uuid::Uuid::now_v7(),
        "live-codex-resume",
    ));
    let backend = CodexConnection::new(spawner)
        .open_session(
            SessionSpec::Resume {
                session_id: "live-codex-resume".into(),
                backend_session_id: Some(thread_id),
            },
            SessionConfig {
                cli_program: Some(binary.into()),
                cwd: Some(directory.path().to_string_lossy().into_owned()),
                extra_args: vec!["-c".into(), "mcp_servers={}".into()],
                spawn_env: vec![
                    EnvVar {
                        name: "CODEX_HOME".into(),
                        value: user_home.join(".codex").to_string_lossy().into_owned(),
                    },
                    EnvVar {
                        name: "CODEX_ACCOUNTS_PATH".into(),
                        value: test_registry.to_string_lossy().into_owned(),
                    },
                ],
                ..Default::default()
            },
        )
        .await
        .expect("original account resumes the existing native thread");
    accounts["active"] = serde_json::json!(target);
    std::fs::write(&test_registry, accounts.to_string()).unwrap();
    // Account refresh occurs before prompt validation. An unsupported audio block
    // exercises the production restart/login/resume path without asking a model to work.
    let result = backend
        .dispatch(Command::Send {
            content: vec![ContentBlock::Audio {
                data: Vec::new(),
                media_type: "audio/wav".into(),
            }],
            metadata: CommandMeta::default(),
        })
        .await;
    assert!(
        matches!(result, Err(BackendError::CommandNotSupported { .. })),
        "account switch and resume must complete before prompt validation"
    );
    let mut events = backend.events();
    backend.dispatch(Command::ListCheckpoints).await.unwrap();
    let restored = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(envelope) = events.next().await {
            if let aionui_session::SessionEvent::CheckpointList { entries } = envelope.event {
                return entries.len();
            }
        }
        0
    })
    .await
    .unwrap();
    backend.terminate().await;
    assert!(
        restored > 0,
        "the original turns remain available after switching accounts"
    );
}
