//! Supply the selected profile's credentials to a process using the original history home.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aionui_common::CommandSpec;
use aionui_process::Spawner;
use base64::Engine;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::adapter::{AgentIo, ManagedProcessIo};

#[derive(Clone)]
pub(crate) struct CodexAuthProvider {
    profile_home: PathBuf,
    spawner: Arc<dyn Spawner>,
    command: CommandSpec,
}

// Deliberately no Debug/Serialize: credentials must never enter logs or persisted state.
struct Credentials {
    access_token: String,
    account_id: String,
    expires_at: Option<u64>,
}

impl CodexAuthProvider {
    pub(crate) fn new(profile_home: PathBuf, spawner: Arc<dyn Spawner>, mut command: CommandSpec) -> Self {
        command.args = vec!["app-server".into(), "-c".into(), "mcp_servers={}".into()];
        Self {
            profile_home,
            spawner,
            command,
        }
    }

    pub(crate) async fn login_params(&self, force_refresh: bool) -> Result<Value, &'static str> {
        let mut credentials = self.read_credentials().await?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if force_refresh || credentials.expires_at.is_some_and(|expiry| expiry <= now + 60) {
            self.refresh_profile().await?;
            credentials = self.read_credentials().await?;
            if credentials.expires_at.is_some_and(|expiry| expiry <= now) {
                return Err("the selected Codex account login has expired; sign in to that account again");
            }
        }
        Ok(json!({
            "type": "chatgptAuthTokens",
            "accessToken": credentials.access_token,
            "chatgptAccountId": credentials.account_id,
        }))
    }

    async fn read_credentials(&self) -> Result<Credentials, &'static str> {
        let bytes = tokio::fs::read(self.profile_home.join("auth.json"))
            .await
            .map_err(|_| "could not read the selected Codex account login; sign in to that account again")?;
        parse_credentials(&bytes)
    }

    /// Use Codex's own OAuth implementation and refresh lock for the selected profile.
    /// No copied refresh tokens, custom OAuth client, or writes to the history profile.
    async fn refresh_profile(&self) -> Result<(), &'static str> {
        let process = self
            .spawner
            .spawn(self.command.clone(), &[], "aionui-session-auth")
            .await
            .map_err(|_| "could not refresh the selected Codex account login")?;
        let io = ManagedProcessIo::new(process);
        let operation = async {
            let (mut stdin, stdout) = io.take_stdio().await.ok_or("Codex account refresh has no transport")?;
            let mut lines = BufReader::new(stdout).lines();
            for (id, method, params) in [
                (
                    1,
                    "initialize",
                    json!({"clientInfo":{"name":"aionui-session-auth","version":"0.1.0"},"capabilities":{"experimentalApi":true,"requestAttestation":false}}),
                ),
                // Verified: Codex app-server account/read `refreshToken` (managed auth).
                (2, "account/read", json!({"refreshToken":true})),
            ] {
                let mut frame = serde_json::to_vec(&json!({"id":id,"method":method,"params":params})).unwrap();
                frame.push(b'\n');
                stdin
                    .write_all(&frame)
                    .await
                    .map_err(|_| "Codex account refresh transport closed")?;
                stdin
                    .flush()
                    .await
                    .map_err(|_| "Codex account refresh transport closed")?;
                loop {
                    let line = lines
                        .next_line()
                        .await
                        .map_err(|_| "Codex account refresh transport closed")?
                        .ok_or("Codex account refresh process exited")?;
                    let response: Value =
                        serde_json::from_str(&line).map_err(|_| "invalid Codex account refresh response")?;
                    if response["id"] != id {
                        continue;
                    }
                    if response.get("error").is_some() {
                        return Err("Codex could not refresh the selected account; sign in again");
                    }
                    if id == 2 && response["result"]["account"].is_null() {
                        return Err("the selected Codex account is signed out");
                    }
                    break;
                }
            }
            Ok(())
        };
        // External-token reverse RPCs have a short deadline; never strand the active turn.
        let result = tokio::time::timeout(Duration::from_secs(8), operation)
            .await
            .map_err(|_| "refreshing the selected Codex account timed out")
            .and_then(|result| result);
        io.terminate().await;
        result
    }
}

fn parse_credentials(bytes: &[u8]) -> Result<Credentials, &'static str> {
    let value: Value = serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
        .map_err(|_| "the selected Codex account login is invalid")?;
    let tokens = &value["tokens"];
    let access_token = tokens["access_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or("the selected profile has no ChatGPT login; sign in to that account again")?
        .to_owned();
    let account_id = tokens["account_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("the selected Codex account identity is missing; sign in again")?
        .to_owned();
    let expires_at = access_token
        .split('.')
        .nth(1)
        .and_then(|payload| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok())
        .and_then(|payload| serde_json::from_slice::<Value>(&payload).ok())
        .and_then(|claims| claims["exp"].as_u64());
    Ok(Credentials {
        access_token,
        account_id,
        expires_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_login_without_echoing_secrets_in_errors() {
        let error = parse_credentials(br#"{"tokens":{"access_token":"private-token"}}"#)
            .err()
            .unwrap();
        assert!(error.contains("identity is missing"));
        assert!(!error.contains("private-token"));
    }

    #[test]
    fn extracts_chatgpt_identity_and_token_expiry() {
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":1900000000}"#);
        let credentials = parse_credentials(
            json!({"tokens":{"access_token":format!("header.{claims}.signature"),"account_id":"account-b"}})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(credentials.account_id, "account-b");
        assert_eq!(credentials.expires_at, Some(1900000000));
    }

    #[tokio::test]
    async fn expired_login_refreshes_only_the_selected_profile_through_the_native_cli() {
        let root = tempfile::tempdir().unwrap();
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":1}"#);
        tokio::fs::write(root.path().join("auth.json"), json!({"tokens":{"access_token":format!("header.{claims}.signature"),"account_id":"b","refresh_token":"private-refresh"}}).to_string()).await.unwrap();
        let spawner = Arc::new(crate::testing::FakeSpawner::new());
        let provider = CodexAuthProvider::new(
            root.path().to_owned(),
            spawner.clone(),
            CommandSpec {
                command: "codex".into(),
                args: Vec::new(),
                cwd: None,
                env: vec![aionui_common::EnvVar {
                    name: "CODEX_HOME".into(),
                    value: root.path().to_string_lossy().into_owned(),
                }],
            },
        );
        let error = provider.login_params(false).await.unwrap_err();
        assert_eq!(error, "could not refresh the selected Codex account login");
        assert_eq!(spawner.call_count(), 1);
        assert!(
            spawner
                .last_command()
                .await
                .unwrap()
                .env
                .iter()
                .any(|entry| entry.name == "CODEX_HOME" && std::path::Path::new(&entry.value) == root.path())
        );
    }
}
