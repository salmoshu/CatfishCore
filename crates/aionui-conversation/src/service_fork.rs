use super::*;
use aionui_db::ConversationForkSeed;
use sha2::{Digest, Sha256};

impl ConversationService {
    /// Snapshot a source without acquiring its turn, stopping its runtime or touching its queue.
    #[tracing::instrument(skip_all, fields(conversation_id = %id))]
    pub async fn fork(
        &self,
        user_id: &str,
        id: &str,
        req: ForkConversationRequest,
    ) -> Result<ConversationResponse, ConversationError> {
        let operation_id = req.operation_id.clone().unwrap_or_else(generate_short_id);
        if operation_id.is_empty()
            || operation_id.len() > 128
            || !operation_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(ConversationError::BadRequest {
                reason: "FORK_INVALID_OPERATION: invalid operation id".into(),
            });
        }
        let child_id = format!("fork_{:x}", Sha256::digest(format!("{user_id}\0{operation_id}")));
        let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&serde_json::json!({
            "parent": id, "message": req.message_id, "target": req.target_agent_id, "name": req.name, "continue": req.continue_task,
        })).map_err(|e| ConversationError::internal(e.to_string()))?));
        if let Some(existing) = self.conversation_repo.get(user_id, &child_id).await? {
            return self
                .finish_fork(user_id, existing, &fingerprint, req.continue_task)
                .await;
        }
        let parent = self
            .conversation_repo
            .get(user_id, id)
            .await?
            .ok_or_else(|| ConversationError::NotFound { id: id.into() })?;
        if team_id_from_extra(&parent.extra).is_some() {
            return Err(ConversationError::Forbidden {
                reason: "team conversations cannot be forked".into(),
            });
        }
        let fork_point = match self.conversation_repo.get_message(user_id, id, &req.message_id).await? {
            Some(row) => row,
            None => self
                .conversation_repo
                .get_message_by_msg_id_any(user_id, id, &req.message_id)
                .await?
                .ok_or_else(|| ConversationError::MessageNotFound {
                    id: req.message_id.clone(),
                })?,
        };
        let parent_extra: serde_json::Value =
            serde_json::from_str(&parent.extra).map_err(|e| ConversationError::internal(e.to_string()))?;
        let acp = self.acp_session_repo.get_for_user(user_id, id).await?;
        let source_agent = match &acp {
            Some(row) => row.agent_id.clone(),
            None if parse_agent_type_from_row(&parent) == Some(AgentType::Aionrs) => {
                self.aionrs_capability_agent_id(user_id, id).await?
            }
            None => {
                let selector = parent_extra
                    .get("agent_id")
                    .or_else(|| parent_extra.get("backend"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                self.resolve_assistant_agent_binding(user_id, selector)
                    .await?
                    .map(|binding| binding.agent_id)
                    .unwrap_or_default()
            }
        };
        let target_id = req.target_agent_id.as_deref().unwrap_or(&source_agent);
        let binding = self
            .resolve_assistant_agent_binding(user_id, target_id)
            .await?
            .ok_or_else(|| ConversationError::BadRequest {
                reason: "FORK_TARGET_UNAVAILABLE: target agent is not registered".into(),
            })?;
        let metadata = self
            .agent_metadata_repo
            .get_for_user(user_id, &binding.agent_id)
            .await?;
        if metadata.as_ref().is_none_or(|row| !row.enabled) {
            return Err(ConversationError::BadRequest {
                reason: "FORK_TARGET_UNAVAILABLE: target agent is disabled".into(),
            });
        }
        let same_agent = binding.agent_id == source_agent;
        let capability = self
            .fork_capability_for_agent(user_id, &source_agent, &parent.extra)
            .await?;
        let parent_sid = acp
            .as_ref()
            .and_then(|row| row.session_id.clone())
            .or_else(|| (parse_agent_type_from_row(&parent) == Some(AgentType::Aionrs)).then(|| id.to_owned()));
        let native = same_agent && capability.is_some() && parent_sid.is_some();
        let cursor = (fork_point.created_at, fork_point.id.as_str());
        let latest = self
            .conversation_repo
            .list_messages_page(
                user_id,
                id,
                &MessagePageParams {
                    limit: 1,
                    direction: MessagePageDirection::InitialLatest,
                },
            )
            .await?;
        let is_head = latest.items.last().is_none_or(|message| message.id == fork_point.id);
        let anchor = if native && capability.is_some_and(|c| c.at_turn) {
            self.conversation_repo
                .resolve_backend_turn_anchor(user_id, id, cursor)
                .await?
        } else {
            None
        };
        let live_head = if native && !is_head && anchor.is_some() && self.runtime_state.active_turn_id_for(id).is_some()
        {
            if let Some(last) = latest.items.last() {
                self.conversation_repo
                    .resolve_backend_turn_anchor(user_id, id, (last.created_at, last.id.as_str()))
                    .await?
                    == anchor
            } else {
                false
            }
        } else {
            false
        };
        // Verified: Codex 0.167.0 self-schema v2/ThreadForkParams.json says lastTurnId
        // cannot name an in-progress turn. Snapshot live HEAD without that parameter.
        let last_turn_id = if native && !is_head && !live_head {
            if !capability.is_some_and(|c| c.at_turn) {
                return Err(ConversationError::Unprocessable {
                    reason: "FORK_POINT_UNSUPPORTED: this agent only supports forking from the latest message".into(),
                });
            }
            Some(anchor.ok_or_else(|| ConversationError::Unprocessable {
                reason: "FORK_POINT_UNSUPPORTED: this message predates turn tracking; fork from the latest message instead".into(),
            })?)
        } else {
            None
        };

        // Allowlist configuration; session ids, runtime snapshots, capability caches, cron and queues never cross.
        let mut extra = serde_json::Map::new();
        for key in ["workspace", "customWorkspace"] {
            if let Some(value) = parent_extra.get(key) {
                extra.insert(key.into(), value.clone());
            }
        }
        if same_agent {
            for key in [
                "preset_assistant_id",
                "preset_context",
                "preset_rules",
                "system_prompt",
                "skills",
                "disabled_builtin_skills",
                "mcp_server_ids",
                "session_mode",
                "current_model_id",
                "thought_level",
            ] {
                if let Some(value) = parent_extra.get(key) {
                    extra.insert(key.into(), value.clone());
                }
            }
        }
        extra.insert("agent_id".into(), serde_json::json!(binding.agent_id));
        extra.insert("backend".into(), serde_json::json!(binding.runtime_backend));
        extra.insert("agent_source".into(), serde_json::json!(binding.agent_source));
        let strategy = if native { "native" } else { "handoff" };
        let mut lineage = serde_json::json!({ "parent_conversation_id": id, "parent_message_id": fork_point.id, "strategy": strategy, "operation_id": operation_id });
        if native {
            lineage["parent_session_id"] = serde_json::json!(parent_sid);
            if let Some(turn_id) = last_turn_id {
                lineage["last_turn_id"] = serde_json::json!(turn_id);
            }
        }
        extra.insert("fork".into(), lineage);
        let summary = if native {
            None
        } else {
            Some(self.fork_handoff_summary(user_id, &parent, &fork_point).await?)
        };
        extra.insert("fork_operation".into(), serde_json::json!({
            "id": operation_id, "fingerprint": fingerprint, "delivery": if req.continue_task { "pending" } else { "none" }, "summary": summary,
        }));
        // Seed the fresh agent with the handoff even when the caller chooses to continue manually later.
        if let Some(summary) = &summary {
            let rules = extra
                .get("preset_context")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            extra.insert(
                "preset_context".into(),
                serde_json::json!(format!("{rules}\n\n{summary}")),
            );
            if binding.agent_type == "aionrs" {
                extra.insert("preset_rules".into(), serde_json::json!(summary));
            }
        }
        let now = now_ms();
        let row = ConversationRow {
            id: child_id.clone(),
            user_id: user_id.into(),
            name: req
                .name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| parent.name.clone()),
            name_source: if req.name.is_some() {
                Some("user".into())
            } else {
                parent.name_source.clone()
            },
            r#type: binding.agent_type.clone(),
            extra: serde_json::Value::Object(extra).to_string(),
            model: if same_agent { parent.model.clone() } else { None },
            status: Some("pending".into()),
            source: parent.source.clone(),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: now,
            updated_at: now,
            project_id: parent.project_id.clone(),
            folder_id: parent.folder_id.clone(),
        };
        let handoff_message = summary.as_ref().map(|content| MessageRow {
            id: generate_short_id(),
            conversation_id: child_id.clone(),
            msg_id: None,
            r#type: "text".into(),
            content: serde_json::json!({"content":content}).to_string(),
            position: Some("left".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: now,
            backend_turn_id: None,
        });
        let inserted = self
            .conversation_repo
            .create_fork_once(&ConversationForkSeed {
                row: &row,
                parent_id: id,
                native_cursor: native.then_some(cursor),
                handoff_message: handoff_message.as_ref(),
                agent_identity: (binding.agent_type == "acp")
                    .then_some((binding.agent_source.as_str(), binding.agent_id.as_str())),
                copy_assistant: same_agent,
            })
            .await?;
        if inserted {
            info!(operation_id, parent_conversation_id = id, fork_conversation_id = %child_id, strategy, "Conversation fork snapshot created");
            self.broadcast_list_changed(user_id, &child_id, "created", None);
        }
        let row = self
            .conversation_repo
            .get(user_id, &child_id)
            .await?
            .ok_or_else(|| ConversationError::NotFound { id: child_id })?;
        self.finish_fork(user_id, row, &fingerprint, req.continue_task).await
    }

    async fn finish_fork(
        &self,
        user_id: &str,
        row: ConversationRow,
        fingerprint: &str,
        continue_task: bool,
    ) -> Result<ConversationResponse, ConversationError> {
        let extra: serde_json::Value =
            serde_json::from_str(&row.extra).map_err(|e| ConversationError::internal(e.to_string()))?;
        if extra
            .pointer("/fork_operation/fingerprint")
            .and_then(serde_json::Value::as_str)
            != Some(fingerprint)
        {
            return Err(ConversationError::Busy {
                reason: "FORK_OPERATION_CONFLICT: operation id was already used for a different request".into(),
            });
        }
        // A live source can advance immediately. Open the independent backend snapshot before
        // returning instead of waiting for the new conversation's renderer to mount.
        if extra.pointer("/fork/strategy").and_then(serde_json::Value::as_str) == Some("native")
            && let Some(parent_id) = extra
                .pointer("/fork/parent_conversation_id")
                .and_then(serde_json::Value::as_str)
            && self.task_manager.get_task(parent_id).is_some()
        {
            self.ensure_runtime(user_id, &row.id, &self.task_manager).await?;
        }
        if continue_task && self.conversation_repo.claim_fork_continuation(user_id, &row.id).await? {
            let summary = extra
                .pointer("/fork_operation/summary")
                .and_then(serde_json::Value::as_str);
            let instruction = "Continue the task from the fork snapshot. Review the latest plan, code changes and test evidence, then carry out the next unfinished steps. The source conversation continues independently in the same workspace; inspect current files before editing. Do not stop or modify the source runtime or its queue.";
            let content = match summary {
                Some(summary) => format!("{summary}\n\n{instruction}"),
                None => instruction.into(),
            };
            info!(fork_conversation_id = %row.id, "Fork continuation claimed");
            self.send_message(
                user_id,
                &row.id,
                SendMessageRequest {
                    content,
                    files: vec![],
                    sessions: vec![],
                    inject_skills: vec![],
                    hidden: summary.is_some(),
                },
                &self.task_manager,
            )
            .await?;
        }
        self.get(user_id, &row.id).await
    }

    async fn fork_handoff_summary(
        &self,
        user_id: &str,
        parent: &ConversationRow,
        point: &MessageRow,
    ) -> Result<String, ConversationError> {
        let mut rows = self
            .conversation_repo
            .list_messages_page(
                user_id,
                &parent.id,
                &MessagePageParams {
                    limit: 120,
                    direction: MessagePageDirection::Before {
                        cursor: MessagePageCursor::from(point),
                    },
                },
            )
            .await?
            .items;
        rows.push(point.clone());
        let first = self
            .conversation_repo
            .list_messages_page(
                user_id,
                &parent.id,
                &MessagePageParams {
                    limit: 1,
                    direction: MessagePageDirection::After {
                        cursor: MessagePageCursor {
                            created_at: i64::MIN,
                            id: String::new(),
                        },
                    },
                },
            )
            .await?
            .items;
        let first_goal = first
            .first()
            .filter(|m| m.position.as_deref() == Some("right"))
            .map(message_text)
            .unwrap_or_default();
        let latest_goal = rows
            .iter()
            .rev()
            .find(|m| m.position.as_deref() == Some("right") && m.r#type == "text")
            .map(message_text)
            .unwrap_or_default();
        let plan = self
            .conversation_repo
            .latest_message_of_type(user_id, &parent.id, "plan")
            .await?
            .filter(|m| (m.created_at, m.id.as_str()) <= (point.created_at, point.id.as_str()))
            .or_else(|| rows.iter().rev().find(|m| m.r#type == "plan").cloned());
        let mut changes = Vec::new();
        let mut tests = Vec::new();
        for message in rows.iter().filter(|m| {
            matches!(
                m.r#type.as_str(),
                "tool_call" | "acp_tool_call" | "codex_tool_call" | "text"
            )
        }) {
            let text = message_text(message);
            let lower = text.to_lowercase();
            if [
                "apply_patch",
                "file_path",
                "filediff",
                "*** update file",
                "*** add file",
                "write_file",
                "edit_file",
            ]
            .iter()
            .any(|s| lower.contains(s))
            {
                changes.push(clip(&text, 700));
            }
            if [
                "vitest",
                "cargo test",
                "pytest",
                "bun run test",
                "npm test",
                "tsc --noemit",
                "test result:",
                "tests passed",
                "tests failed",
            ]
            .iter()
            .any(|s| lower.contains(s))
            {
                tests.push(clip(&text, 700));
            }
        }
        let latest_assistant = rows
            .iter()
            .rev()
            .find(|m| m.r#type == "text" && m.position.as_deref() == Some("left"))
            .map(message_text)
            .unwrap_or_default();
        let plan_text = plan
            .as_ref()
            .map(message_text)
            .unwrap_or_else(|| "No structured plan recorded.".into());
        Ok(format!(
            "Task handoff (snapshot; source continues independently)\n\nGoal\n{}\nLatest request\n{}\n\nLatest plan\n{}\n\nCode changes\n{}\n\nTest results\n{}\n\nNext steps\nReview unfinished plan entries and current workspace files. Latest recorded progress:\n{}",
            clip(&first_goal, 1600),
            clip(&latest_goal, 1600),
            clip(&plan_text, 2400),
            recent_evidence(&changes),
            recent_evidence(&tests),
            clip(&latest_assistant, 1600)
        ))
    }
}

fn message_text(row: &MessageRow) -> String {
    serde_json::from_str::<serde_json::Value>(&row.content)
        .ok()
        .and_then(|v| v.get("content").and_then(serde_json::Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| row.content.clone())
}

fn clip(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let text: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{text}…")
    } else {
        text
    }
}

fn recent_evidence(values: &[String]) -> String {
    if values.is_empty() {
        "No matching evidence recorded; verify before assuming success.".into()
    } else {
        values[values.len().saturating_sub(4)..].join("\n")
    }
}
