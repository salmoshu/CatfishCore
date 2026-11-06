//! Explicit live checks of the production CLI adapters; no inference prompts are sent.
//! Set CATFISH_LIVE_CODEX_CLI / CATFISH_LIVE_CLAUDE_CLI to the native binaries.

use std::sync::Arc;

use aionui_process::Spawner;
use aionui_session::{
    BackendConnection, ClaudeConnection, CodexConnection, Command, SessionBackend, SessionConfig, SessionEvent,
    SessionSpec,
};
use futures_util::StreamExt;

fn spawner(directory: &std::path::Path) -> Arc<dyn Spawner> {
    Arc::new(aionui_process::RealSpawner::new(
        Arc::new(aionui_process::FileRegistryStore::new(directory)),
        uuid::Uuid::now_v7(),
        "live-speed-test",
    ))
}

async fn wait_for(backend: &dyn SessionBackend, check: impl Fn(&aionui_session::Capabilities) -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while !check(&backend.capabilities()) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("native catalog/settings must arrive");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "LIVE: requires the native Codex CLI and an authenticated profile"]
async fn codex_speed_switch_preserves_model_and_max_effort() {
    let directory = tempfile::tempdir().unwrap();
    let conn = CodexConnection::new(spawner(directory.path()));
    let backend = conn
        .open_session(
            SessionSpec::Fresh {
                session_id: uuid::Uuid::now_v7().to_string(),
            },
            SessionConfig {
                cwd: Some(directory.path().to_string_lossy().into()),
                cli_program: Some(
                    std::env::var_os("CATFISH_LIVE_CODEX_CLI")
                        .expect("native Codex path")
                        .into(),
                ),
                model: Some("gpt-6.1-sol".into()),
                extra_args: vec!["-c".into(), "service_tier=\"fast\"".into()],
                sandbox_mode: Some("read-only".into()),
                approval_policy: Some("never".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    wait_for(backend.as_ref(), |caps| !caps.available_models.is_empty()).await;
    backend
        .dispatch(Command::SetConfigOption {
            option_id: "effort".into(),
            value: "max".into(),
        })
        .await
        .unwrap();
    wait_for(backend.as_ref(), |caps| caps.current_effort.as_deref() == Some("max")).await;
    let initial = backend.capabilities();
    assert_eq!(initial.current_model.as_deref(), Some("gpt-6.1-sol"));
    for tier in ["default", "priority", "default"] {
        let mut events = backend.events();
        backend
            .dispatch(Command::SetConfigOption {
                option_id: "service_tier".into(),
                value: tier.into(),
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(envelope) = events.next().await {
                if matches!(envelope.event, SessionEvent::ConfigOptionChanged { option_id, value }
                    if option_id == "service_tier" && value.as_deref() == Some(tier))
                {
                    return;
                }
            }
            panic!("speed stream closed");
        })
        .await
        .unwrap();
        let actual = backend.capabilities();
        assert_eq!(actual.current_service_tier.as_deref(), Some(tier));
        assert_eq!(actual.current_model, initial.current_model);
        assert_eq!(actual.current_effort, initial.current_effort);
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "LIVE: requires the native Claude CLI; account availability is reported"]
async fn claude_speed_is_confirmed_or_reports_account_restrictions() {
    let directory = tempfile::tempdir().unwrap();
    let conn = ClaudeConnection::new(spawner(directory.path()));
    let backend = conn
        .open_session(
            SessionSpec::Fresh {
                session_id: uuid::Uuid::now_v7().to_string(),
            },
            SessionConfig {
                cwd: Some(directory.path().to_string_lossy().into()),
                model: Some("opus".into()),
                cli_program: Some(
                    std::env::var_os("CATFISH_LIVE_CLAUDE_CLI")
                        .expect("native Claude path")
                        .into(),
                ),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    wait_for(backend.as_ref(), |caps| !caps.available_models.is_empty()).await;
    let initial = backend.capabilities();
    let outcome = backend
        .dispatch(Command::SetConfigOption {
            option_id: "service_tier".into(),
            value: "fast".into(),
        })
        .await;
    match outcome {
        Ok(_) => assert_eq!(backend.capabilities().current_service_tier.as_deref(), Some("fast")),
        Err(error) => {
            assert!(error.to_string().contains("Claude fast mode unavailable:"), "{error}");
            assert_eq!(backend.capabilities().current_service_tier.as_deref(), Some("default"));
            println!("Native Claude declined fast mode: {error}");
        }
    }
    backend
        .dispatch(Command::SetConfigOption {
            option_id: "service_tier".into(),
            value: "default".into(),
        })
        .await
        .unwrap();
    assert_eq!(backend.capabilities().current_service_tier.as_deref(), Some("default"));
    assert_eq!(backend.capabilities().current_model, initial.current_model);
}
