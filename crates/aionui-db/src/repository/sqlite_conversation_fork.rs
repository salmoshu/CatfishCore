use super::*;
use crate::repository::conversation::ConversationForkSeed;

impl SqliteConversationRepository {
    pub(super) async fn create_atomic_fork(&self, seed: &ConversationForkSeed<'_>) -> Result<bool, DbError> {
        let row = seed.row;
        // The first statement takes the writer lock. The transaction rolls back on cancellation.
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO conversations (id, user_id, name, name_source, type, extra, model, status, source, channel_chat_id, pinned, pinned_at, created_at, updated_at, project_id, folder_id) \
             SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, 0, NULL, ?, ?, ?, ? \
             FROM conversations p WHERE p.id = ? AND p.user_id = ? ON CONFLICT(id) DO NOTHING",
        )
        .bind(&row.id).bind(&row.user_id).bind(&row.name).bind(&row.name_source)
        .bind(&row.r#type).bind(&row.extra).bind(&row.model).bind(&row.status).bind(&row.source)
        .bind(row.created_at).bind(row.updated_at).bind(&row.project_id).bind(&row.folder_id)
        .bind(seed.parent_id).bind(&row.user_id).execute(&mut *tx).await?.rows_affected();
        if inserted == 0 {
            return Ok(false);
        }
        if let Some((source, agent_id)) = seed.agent_identity {
            sqlx::query("INSERT INTO acp_session (conversation_id, agent_source, agent_id, session_id, session_status, session_config, last_active_at) VALUES (?, ?, ?, NULL, 'idle', '{}', ?)")
                .bind(&row.id).bind(source).bind(agent_id).bind(row.created_at).execute(&mut *tx).await?;
        }
        if seed.copy_assistant {
            sqlx::query(
                "INSERT INTO conversation_assistant_snapshots (conversation_id, assistant_definition_id, assistant_id, assistant_source, agent_id, rules_content, default_model_mode, resolved_model_id, default_permission_mode, resolved_permission_value, default_thought_level_mode, resolved_thought_level_value, default_skills_mode, resolved_skill_ids, resolved_disabled_builtin_skill_ids, default_mcps_mode, resolved_mcp_ids, created_at, updated_at) \
                 SELECT ?, assistant_definition_id, assistant_id, assistant_source, agent_id, rules_content, default_model_mode, resolved_model_id, default_permission_mode, resolved_permission_value, default_thought_level_mode, resolved_thought_level_value, default_skills_mode, resolved_skill_ids, resolved_disabled_builtin_skill_ids, default_mcps_mode, resolved_mcp_ids, ?, ? FROM conversation_assistant_snapshots WHERE conversation_id = ?",
            ).bind(&row.id).bind(row.created_at).bind(row.updated_at).bind(seed.parent_id).execute(&mut *tx).await?;
        }
        let messages = if let Some((created_at, id)) = seed.native_cursor {
            sqlx::query_as::<_, MessageRow>("SELECT * FROM messages WHERE conversation_id = ? AND (created_at < ? OR (created_at = ? AND id <= ?)) ORDER BY created_at, id")
                .bind(seed.parent_id).bind(created_at).bind(created_at).bind(id).fetch_all(&mut *tx).await?
        } else {
            seed.handoff_message.cloned().into_iter().collect()
        };
        for message in messages {
            sqlx::query("INSERT INTO messages (id, conversation_id, msg_id, type, content, position, status, hidden, created_at, backend_turn_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)")
                .bind(aionui_common::generate_id()).bind(&row.id).bind(&message.msg_id)
                .bind(&message.r#type).bind(&message.content).bind(&message.position).bind(&message.status)
                .bind(message.hidden).bind(message.created_at).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(true)
    }
}
