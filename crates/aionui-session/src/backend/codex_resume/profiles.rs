//! Resolve local account profiles without moving credentials or conversation history.

use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};

use aionui_common::EnvVar;
use serde::Deserialize;

#[derive(Clone, Debug)]
pub(crate) struct CodexProfiles {
    default_home: Option<PathBuf>,
    explicit_home: Option<PathBuf>,
    accounts_path: Option<PathBuf>,
    accounts_root: Option<PathBuf>,
}

#[derive(Deserialize)]
struct Accounts {
    #[serde(default = "default_account")]
    active: String,
    accounts: Vec<Account>,
    #[serde(default)]
    default_removed: bool,
}

#[derive(Deserialize)]
struct Account {
    id: String,
}

fn default_account() -> String {
    "default".to_owned()
}

fn valid_account_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

impl CodexProfiles {
    pub(crate) fn from_env(env: &[EnvVar]) -> Self {
        let value = |key: &str| {
            env.iter()
                .rev()
                .find(|entry| entry.name.eq_ignore_ascii_case(key))
                .map(|entry| PathBuf::from(&entry.value))
                .or_else(|| std::env::var_os(key).map(PathBuf::from))
        };
        let user_home = value(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
        let config_dir = user_home.as_ref().map(|path| path.join(".config/ai-usage-monitor"));
        Self {
            default_home: user_home.map(|path| path.join(".codex")),
            explicit_home: value("CODEX_HOME"),
            accounts_path: value("CODEX_ACCOUNTS_PATH")
                .or_else(|| config_dir.as_ref().map(|path| path.join("codex-accounts.json"))),
            accounts_root: config_dir.map(|path| path.join("codex-accounts")),
        }
    }

    fn managed_home(&self, path: &Path) -> bool {
        self.default_home.as_deref() == Some(path)
            || self
                .accounts_root
                .as_ref()
                .is_some_and(|root| path.parent() == Some(root.as_path()))
    }

    fn accounts(&self) -> Result<Option<Accounts>, String> {
        let Some(path) = &self.accounts_path else {
            return Ok(None);
        };
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("could not read the Codex account selection".into()),
        };
        let accounts: Accounts = serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes))
            .map_err(|_| "invalid Codex account selection".to_owned())?;
        let mut ids = std::collections::HashSet::new();
        for account in &accounts.accounts {
            if !valid_account_id(&account.id) || account.id == "default" || !ids.insert(&account.id) {
                return Err("invalid registered Codex account".into());
            }
        }
        if !valid_account_id(&accounts.active)
            || if accounts.active == "default" {
                accounts.default_removed
            } else {
                !ids.contains(&accounts.active)
            }
        {
            return Err("the selected Codex account is unavailable".into());
        }
        Ok(Some(accounts))
    }

    /// Follow the account manager only for its own profiles; custom homes stay explicit.
    pub(crate) fn active_home(&self) -> Result<Option<PathBuf>, String> {
        if let Some(path) = &self.explicit_home
            && !self.managed_home(path)
        {
            return Ok(Some(path.clone()));
        }
        match self.accounts()? {
            Some(accounts) if accounts.active != "default" => self
                .accounts_root
                .as_ref()
                .map(|root| Some(root.join(accounts.active)))
                .ok_or_else(|| "could not locate the Codex account directory".into()),
            Some(_) => Ok(self.default_home.clone()),
            None => Ok(self.explicit_home.clone().or_else(|| self.default_home.clone())),
        }
    }

    pub(crate) fn spawn_env(&self, env: &[EnvVar], home: Option<&Path>) -> Vec<EnvVar> {
        let mut env = env.to_vec();
        if let Some(home) = home {
            env.retain(|entry| !entry.name.eq_ignore_ascii_case("CODEX_HOME"));
            env.push(EnvVar {
                name: "CODEX_HOME".into(),
                value: home.to_string_lossy().into_owned(),
            });
        }
        env
    }

    /// Paginated history also needs the originating profile's SQLite thread store.
    pub(crate) fn history_home(&self, rollout: &Path) -> Option<PathBuf> {
        rollout.ancestors().find_map(|ancestor| {
            matches!(
                ancestor.file_name().and_then(|name| name.to_str()),
                Some("sessions" | "archived_sessions")
            )
            .then(|| ancestor.parent().map(Path::to_path_buf))
            .flatten()
        })
    }

    /// Find only the requested UUID inside this OS user's known profile roots.
    pub(crate) fn rollout(&self, thread_id: &str) -> Option<PathBuf> {
        uuid::Uuid::parse_str(thread_id).ok()?;
        let mut homes = Vec::new();
        if let Ok(Some(home)) = self.active_home() {
            homes.push(home);
        }
        homes.extend(self.explicit_home.clone());
        homes.extend(self.default_home.clone());
        if let (Ok(Some(accounts)), Some(root)) = (self.accounts(), &self.accounts_root) {
            homes.extend(accounts.accounts.into_iter().map(|account| root.join(account.id)));
        }
        let mut seen = std::collections::HashSet::new();
        for home in homes {
            if !seen.insert(home.clone()) {
                continue;
            }
            for directory in ["sessions", "archived_sessions"] {
                if let Some(path) = find_rollout(&home.join(directory), thread_id, 4) {
                    return Some(path);
                }
            }
        }
        None
    }
}

fn find_rollout(directory: &Path, thread_id: &str, depth: u8) -> Option<PathBuf> {
    let entries = std::fs::read_dir(directory).ok()?;
    let suffix = format!("-{thread_id}.jsonl");
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        let path = entry.path();
        if kind.is_dir() && depth > 0 {
            if let Some(found) = find_rollout(&path, thread_id, depth - 1) {
                return Some(found);
            }
        } else if kind.is_file() && entry.file_name().to_string_lossy().ends_with(&suffix) {
            // `path` overrides `threadId` in the native API: verify the file's identity.
            let Ok(file) = std::fs::File::open(&path) else { continue };
            let mut first_line = String::new();
            let Ok(_) = std::io::BufReader::new(file)
                .take(1024 * 1024)
                .read_line(&mut first_line)
            else {
                continue;
            };
            let Ok(meta) = serde_json::from_str::<serde_json::Value>(&first_line) else {
                continue;
            };
            if meta["type"] == "session_meta" && meta["payload"]["id"].as_str() == Some(thread_id) {
                return std::fs::canonicalize(path).ok();
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profiles(root: &Path) -> CodexProfiles {
        CodexProfiles {
            default_home: Some(root.join(".codex")),
            explicit_home: Some(root.join("profiles/a")),
            accounts_path: Some(root.join("accounts.json")),
            accounts_root: Some(root.join("profiles")),
        }
    }

    fn select(root: &Path, active: &str) {
        std::fs::write(
            root.join("accounts.json"),
            serde_json::json!({"active": active, "accounts": [{"id": "a"}, {"id": "b"}]}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn follows_account_changes_without_changing_the_parent_environment() {
        let root = tempfile::tempdir().unwrap();
        let profiles = profiles(root.path());
        select(root.path(), "a");
        assert_eq!(profiles.active_home().unwrap(), Some(root.path().join("profiles/a")));
        select(root.path(), "b");
        assert_eq!(profiles.active_home().unwrap(), Some(root.path().join("profiles/b")));
    }

    #[test]
    fn custom_home_does_not_follow_the_account_manager() {
        let root = tempfile::tempdir().unwrap();
        let mut profiles = profiles(root.path());
        profiles.explicit_home = Some(root.path().join("custom"));
        select(root.path(), "b");
        assert_eq!(profiles.active_home().unwrap(), profiles.explicit_home);
    }

    #[test]
    fn invalid_account_selection_is_rejected_without_falling_back_to_another_account() {
        let root = tempfile::tempdir().unwrap();
        let profiles = profiles(root.path());
        select(root.path(), "../outside");
        assert_eq!(
            profiles.active_home().unwrap_err(),
            "the selected Codex account is unavailable"
        );
    }

    #[test]
    fn locates_the_original_rollout_after_switching_accounts_and_checks_its_identity() {
        let root = tempfile::tempdir().unwrap();
        let profiles = profiles(root.path());
        select(root.path(), "b");
        let id = uuid::Uuid::new_v4().to_string();
        let directory = root.path().join("profiles/a/sessions/2026/11/10");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("rollout-2026-11-10T12-00-00-{id}.jsonl"));
        std::fs::write(
            &path,
            serde_json::json!({"type":"session_meta", "payload":{"id":id}}).to_string(),
        )
        .unwrap();
        assert_eq!(profiles.rollout(&id), Some(std::fs::canonicalize(&path).unwrap()));
        std::fs::write(&path, r#"{"type":"session_meta","payload":{"id":"different"}}"#).unwrap();
        assert_eq!(profiles.rollout(&id), None);
    }

    #[test]
    fn missing_or_invalid_thread_never_selects_an_unrelated_rollout() {
        let root = tempfile::tempdir().unwrap();
        let profiles = profiles(root.path());
        select(root.path(), "b");
        assert!(profiles.rollout("../../outside").is_none());
        assert!(profiles.rollout(&uuid::Uuid::new_v4().to_string()).is_none());
    }
}
