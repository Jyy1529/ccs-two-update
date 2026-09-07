//! SQLite access for provider groups and reusable balance query templates.

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::provider::{Provider, ProviderMeta};
use crate::provider_groups::{
    BalanceQueryTemplate, KeyPoolStrategy, ProviderGroup, ProviderGroupKind,
};
use rusqlite::{params, OptionalExtension};
use std::str::FromStr;

fn enabled_pool_members(
    conn: &rusqlite::Connection,
    app: &str,
    group_id: &str,
) -> Result<Vec<Provider>, AppError> {
    let mut statement = conn.prepare(
        "SELECT id, name, settings_config, meta, category FROM providers
        WHERE app_type = ?1 AND json_extract(meta, '$.providerGroupId') = ?2
        AND json_extract(meta, '$.keyPoolEnabled') = 1 ORDER BY id",
    )?;
    let rows = statement.query_map(params![app, group_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    rows.map(|row| {
        let (id, name, config, meta, category) = row?;
        let mut provider = Provider::with_id(
            id,
            name,
            serde_json::from_str(&config)
                .map_err(|_| AppError::Database("Invalid provider configuration".into()))?,
            None,
        );
        provider.meta = Some(
            serde_json::from_str(&meta)
                .map_err(|_| AppError::Database("Invalid provider metadata".into()))?,
        );
        provider.category = category;
        Ok(provider)
    })
    .collect()
}

/// Must run while the caller holds its write transaction/connection lock.
pub(crate) fn validate_pool_assignment(
    conn: &rusqlite::Connection,
    app: &str,
    provider: &Provider,
) -> Result<(), AppError> {
    let enabled = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.key_pool_enabled)
        .unwrap_or(false);
    let group_id = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.provider_group_id.as_deref());
    let Some(group_id) = group_id else {
        return if enabled {
            Err(AppError::InvalidInput(
                "[pool_group_required] Select a folder before joining a Key pool".into(),
            ))
        } else {
            Ok(())
        };
    };
    let group: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT app_type, normalized_base_url FROM provider_groups WHERE id = ?1",
            params![group_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((group_app, expected_base)) = group else {
        return Err(AppError::InvalidInput(
            "[pool_group_required] Provider group does not exist".into(),
        ));
    };
    if group_app != app {
        return Err(AppError::InvalidInput(
            "[pool_app_mismatch] Provider and folder must belong to the same app".into(),
        ));
    }
    if !enabled {
        return Ok(());
    }
    let mut members = enabled_pool_members(conn, app, group_id)?;
    members.retain(|member| member.id != provider.id);
    members.push(provider.clone());
    crate::provider_groups::validate_key_pool_members(
        &crate::app_config::AppType::from_str(app)?,
        expected_base.as_deref(),
        &members,
    )
}

fn validate_order(expected: Vec<String>, ids: &[String]) -> Result<(), AppError> {
    let expected: std::collections::HashSet<_> = expected.iter().collect();
    let actual: std::collections::HashSet<_> = ids.iter().collect();
    if ids.len() != actual.len() || expected != actual {
        return Err(AppError::InvalidInput(
            "Order must include every item exactly once within its app or folder".into(),
        ));
    }
    Ok(())
}

fn group_kind_to_db(kind: ProviderGroupKind) -> &'static str {
    match kind {
        ProviderGroupKind::Manual => "manual",
        ProviderGroupKind::AutoBaseUrl => "auto_base_url",
    }
}

fn group_kind_from_db(value: &str) -> Result<ProviderGroupKind, AppError> {
    match value {
        "manual" => Ok(ProviderGroupKind::Manual),
        "auto_base_url" => Ok(ProviderGroupKind::AutoBaseUrl),
        other => Err(AppError::Database(format!(
            "Unknown provider group kind: {other}"
        ))),
    }
}

fn strategy_to_db(strategy: KeyPoolStrategy) -> &'static str {
    match strategy {
        KeyPoolStrategy::Failover => "failover",
        KeyPoolStrategy::RoundRobin => "round_robin",
    }
}

fn strategy_from_db(value: &str) -> Result<KeyPoolStrategy, AppError> {
    match value {
        "failover" => Ok(KeyPoolStrategy::Failover),
        "round_robin" => Ok(KeyPoolStrategy::RoundRobin),
        other => Err(AppError::Database(format!(
            "Unknown provider key pool strategy: {other}"
        ))),
    }
}

fn validate_group(group: &ProviderGroup) -> Result<(), AppError> {
    if group.id.trim().is_empty() || group.id.len() > 128 {
        return Err(AppError::InvalidInput(
            "Provider group ID must be 1-128 characters".to_string(),
        ));
    }
    if group.app_type.trim().is_empty() || group.app_type.len() > 64 {
        return Err(AppError::InvalidInput(
            "Provider group app type is invalid".to_string(),
        ));
    }
    if group.name.trim().is_empty() || group.name.chars().count() > 120 {
        return Err(AppError::InvalidInput(
            "Provider group name must be 1-120 characters".to_string(),
        ));
    }
    if group.key_pool_max_retries > 100 {
        return Err(AppError::InvalidInput(
            "Key pool retries must be between 0 and 100".to_string(),
        ));
    }
    if group.icon.as_deref().is_some_and(|icon| {
        !matches!(
            icon,
            "folder" | "star" | "server" | "globe" | "briefcase" | "archive"
        )
    }) {
        return Err(AppError::InvalidInput("Invalid folder icon".into()));
    }
    if group.icon_color.as_deref().is_some_and(|color| {
        color.len() != 7
            || !color.starts_with('#')
            || !color[1..].bytes().all(|c| c.is_ascii_hexdigit())
    }) {
        return Err(AppError::InvalidInput(
            "Folder color must be #RRGGBB".into(),
        ));
    }
    if group.key_pool_cooldown_ms > 60_000 {
        return Err(AppError::InvalidInput(
            "Key pool cooldown must be between 0 and 60000 ms".to_string(),
        ));
    }
    if matches!(group.kind, ProviderGroupKind::AutoBaseUrl)
        && group
            .normalized_base_url
            .as_deref()
            .is_none_or(str::is_empty)
    {
        return Err(AppError::InvalidInput(
            "Automatic provider groups require a normalized Base URL".to_string(),
        ));
    }
    Ok(())
}

type GroupRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    usize,
    bool,
    bool,
    String,
    u32,
    u64,
    Option<String>,
    i64,
    i64,
    Option<String>,
    Option<String>,
);

fn group_from_row(row: GroupRow) -> Result<ProviderGroup, AppError> {
    let (
        id,
        app_type,
        name,
        kind,
        normalized_base_url,
        sort_index,
        collapsed,
        key_pool_enabled,
        strategy,
        key_pool_max_retries,
        key_pool_cooldown_ms,
        balance_template_id,
        created_at,
        updated_at,
        icon,
        icon_color,
    ) = row;
    Ok(ProviderGroup {
        id,
        app_type,
        name,
        kind: group_kind_from_db(&kind)?,
        icon,
        icon_color,
        normalized_base_url,
        sort_index,
        collapsed,
        key_pool_enabled,
        key_pool_strategy: strategy_from_db(&strategy)?,
        key_pool_max_retries,
        key_pool_cooldown_ms,
        balance_template_id,
        created_at,
        updated_at,
    })
}

impl Database {
    pub fn list_provider_groups(&self, app_type: &str) -> Result<Vec<ProviderGroup>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut statement = conn.prepare(
            "SELECT id, app_type, name, kind, normalized_base_url, sort_index,
                    collapsed, key_pool_enabled, key_pool_strategy, key_pool_max_retries,
                    key_pool_cooldown_ms, balance_template_id, created_at, updated_at, icon, icon_color
             FROM provider_groups WHERE app_type = ?1 ORDER BY sort_index ASC, id ASC",
        )?;
        let rows = statement.query_map(params![app_type], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
                row.get(11)?,
                row.get(12)?,
                row.get(13)?,
                row.get(14)?,
                row.get(15)?,
            ))
        })?;
        rows.map(|row| group_from_row(row?)).collect()
    }

    pub fn get_provider_group(&self, id: &str) -> Result<Option<ProviderGroup>, AppError> {
        let conn = lock_conn!(self.conn);
        let row = conn
            .query_row(
                "SELECT id, app_type, name, kind, normalized_base_url, sort_index,
                        collapsed, key_pool_enabled, key_pool_strategy, key_pool_max_retries,
                        key_pool_cooldown_ms, balance_template_id, created_at, updated_at, icon, icon_color
                 FROM provider_groups WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                        row.get(11)?,
                        row.get(12)?,
                        row.get(13)?,
                        row.get(14)?,
                        row.get(15)?,
                    ))
                },
            )
            .optional()?;
        row.map(group_from_row).transpose()
    }

    pub fn create_provider_group(&self, group: &ProviderGroup) -> Result<ProviderGroup, AppError> {
        validate_group(group)?;
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT INTO provider_groups (
                id, app_type, name, kind, normalized_base_url, sort_index, collapsed,
                key_pool_enabled, key_pool_strategy, key_pool_max_retries,
                key_pool_cooldown_ms, balance_template_id, created_at, updated_at, icon, icon_color
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                group.id,
                group.app_type,
                group.name.trim(),
                group_kind_to_db(group.kind),
                group.normalized_base_url,
                group.sort_index,
                group.collapsed,
                group.key_pool_enabled,
                strategy_to_db(group.key_pool_strategy),
                group.key_pool_max_retries,
                group.key_pool_cooldown_ms,
                group.balance_template_id,
                group.created_at,
                group.updated_at,
                group.icon,
                group.icon_color,
            ],
        )?;
        Ok(group.clone())
    }

    pub fn update_provider_group(&self, group: &ProviderGroup) -> Result<(), AppError> {
        validate_group(group)?;
        let conn = lock_conn!(self.conn);
        let members = enabled_pool_members(&conn, &group.app_type, &group.id)?;
        crate::provider_groups::validate_key_pool_members(
            &crate::app_config::AppType::from_str(&group.app_type)?,
            group.normalized_base_url.as_deref(),
            &members,
        )?;
        if group.key_pool_enabled && members.is_empty() {
            return Err(AppError::InvalidInput(
                "[pool_empty] Enable at least one eligible Key member first".into(),
            ));
        }
        let updated = conn.execute(
            "UPDATE provider_groups SET
                app_type = ?1, name = ?2, kind = ?3, normalized_base_url = ?4,
                sort_index = ?5, collapsed = ?6, key_pool_enabled = ?7,
                key_pool_strategy = ?8, key_pool_max_retries = ?9,
                key_pool_cooldown_ms = ?10, balance_template_id = ?11, updated_at = ?12,
                icon = ?14, icon_color = ?15
             WHERE id = ?13 AND app_type = ?1",
            params![
                group.app_type,
                group.name.trim(),
                group_kind_to_db(group.kind),
                group.normalized_base_url,
                group.sort_index,
                group.collapsed,
                group.key_pool_enabled,
                strategy_to_db(group.key_pool_strategy),
                group.key_pool_max_retries,
                group.key_pool_cooldown_ms,
                group.balance_template_id,
                group.updated_at,
                group.id,
                group.icon,
                group.icon_color,
            ],
        )?;
        if updated != 1 {
            return Err(AppError::Database(format!(
                "Provider group '{}' does not exist",
                group.id
            )));
        }
        Ok(())
    }

    pub fn find_auto_provider_group(
        &self,
        app_type: &str,
        normalized_base_url: &str,
    ) -> Result<Option<ProviderGroup>, AppError> {
        let conn = lock_conn!(self.conn);
        let row = conn
            .query_row(
                "SELECT id, app_type, name, kind, normalized_base_url, sort_index,
                        collapsed, key_pool_enabled, key_pool_strategy, key_pool_max_retries,
                        key_pool_cooldown_ms, balance_template_id, created_at, updated_at, icon, icon_color
                 FROM provider_groups
                 WHERE app_type = ?1 AND kind = 'auto_base_url' AND normalized_base_url = ?2",
                params![app_type, normalized_base_url],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                        row.get(11)?,
                        row.get(12)?,
                        row.get(13)?,
                        row.get(14)?,
                        row.get(15)?,
                    ))
                },
            )
            .optional()?;
        row.map(group_from_row).transpose()
    }

    pub fn delete_provider_group(&self, id: &str) -> Result<(), AppError> {
        let mut conn = lock_conn!(self.conn);
        let tx = conn.transaction()?;
        let app_type: Option<String> = tx
            .query_row(
                "SELECT app_type FROM provider_groups WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(app_type) = app_type else {
            return Err(AppError::Database(format!(
                "Provider group '{id}' does not exist"
            )));
        };

        tx.execute(
            "UPDATE providers SET meta = json_set(json_remove(meta,
             '$.providerGroupId', '$.providerGroupSortIndex', '$.keyPoolEnabled'), '$.providerGroupManual', json('true'))
             WHERE app_type = ?1 AND json_extract(meta, '$.providerGroupId') = ?2",
            params![app_type, id],
        )?;

        tx.execute("DELETE FROM provider_groups WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn assign_provider_group(
        &self,
        app_type: &str,
        provider_id: &str,
        group_id: Option<&str>,
        sort_index: Option<usize>,
        key_pool_enabled: Option<bool>,
        manual_assignment: Option<bool>,
    ) -> Result<(), AppError> {
        let mut conn = lock_conn!(self.conn);
        let tx = conn.transaction()?;

        let current_meta: String = tx
            .query_row(
                "SELECT meta FROM providers WHERE id = ?1 AND app_type = ?2",
                params![provider_id, app_type],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| {
                AppError::Database(format!(
                    "Provider '{provider_id}' does not exist in app '{app_type}'"
                ))
            })?;

        if let Some(group_id) = group_id {
            let group_app: Option<String> = tx
                .query_row(
                    "SELECT app_type FROM provider_groups WHERE id = ?1",
                    params![group_id],
                    |row| row.get(0),
                )
                .optional()?;
            if group_app.as_deref() != Some(app_type) {
                return Err(AppError::InvalidInput(
                    "Provider and group must belong to the same app".to_string(),
                ));
            }
        }

        let mut value: serde_json::Value = serde_json::from_str(&current_meta)
            .map_err(|_| AppError::Database("Invalid provider metadata".into()))?;
        let meta = value
            .as_object_mut()
            .ok_or_else(|| AppError::Database("Invalid provider metadata".into()))?;
        let moved = meta.get("providerGroupId").and_then(|v| v.as_str()) != group_id;
        if let Some(group_id) = group_id {
            meta.insert("providerGroupId".into(), serde_json::json!(group_id));
            if moved {
                meta.remove("providerGroupSortIndex");
                meta.remove("keyPoolEnabled");
            }
            if let Some(index) = sort_index {
                meta.insert("providerGroupSortIndex".into(), serde_json::json!(index));
            }
            if let Some(enabled) = key_pool_enabled {
                meta.insert("keyPoolEnabled".into(), serde_json::json!(enabled));
            }
        } else {
            meta.remove("providerGroupId");
            meta.remove("providerGroupSortIndex");
            meta.remove("keyPoolEnabled");
        }
        if let Some(manual) = manual_assignment {
            meta.insert("providerGroupManual".into(), serde_json::json!(manual));
        }
        let serialized = serde_json::to_string(&value)
            .map_err(|error| AppError::Database(format!("Serialize provider metadata: {error}")))?;
        tx.execute(
            "UPDATE providers SET meta = ?1 WHERE id = ?2 AND app_type = ?3",
            params![serialized, provider_id, app_type],
        )?;
        if key_pool_enabled == Some(true) {
            let member = enabled_pool_members(&tx, app_type, group_id.unwrap_or_default())?
                .into_iter()
                .find(|member| member.id == provider_id)
                .ok_or_else(|| {
                    AppError::InvalidInput("[pool_group_required] Select a folder first".into())
                })?;
            validate_pool_assignment(&tx, app_type, &member)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn provider_group_id(
        &self,
        app_type: &str,
        provider_id: &str,
    ) -> Result<Option<String>, AppError> {
        let conn = lock_conn!(self.conn);
        let meta_json: Option<String> = conn
            .query_row(
                "SELECT meta FROM providers WHERE id = ?1 AND app_type = ?2",
                params![provider_id, app_type],
                |row| row.get(0),
            )
            .optional()?;
        let Some(meta_json) = meta_json else {
            return Ok(None);
        };
        let meta: ProviderMeta = serde_json::from_str(&meta_json).unwrap_or_default();
        Ok(meta.provider_group_id)
    }

    pub fn reorder_provider_groups(&self, app_type: &str, ids: &[String]) -> Result<(), AppError> {
        let mut conn = lock_conn!(self.conn);
        let tx = conn.transaction()?;
        let expected = tx
            .prepare("SELECT id FROM provider_groups WHERE app_type = ?1")?
            .query_map(params![app_type], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        validate_order(expected, ids)?;
        for (index, id) in ids.iter().enumerate() {
            tx.execute("UPDATE provider_groups SET sort_index = ?1, updated_at = ?2 WHERE id = ?3 AND app_type = ?4",
                params![index, chrono::Utc::now().timestamp_millis(), id, app_type])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn reorder_provider_group_members(
        &self,
        group_id: &str,
        ids: &[String],
    ) -> Result<(), AppError> {
        let mut conn = lock_conn!(self.conn);
        let tx = conn.transaction()?;
        let app: String = tx.query_row(
            "SELECT app_type FROM provider_groups WHERE id = ?1",
            params![group_id],
            |row| row.get(0),
        )?;
        let expected = tx.prepare("SELECT id FROM providers WHERE app_type = ?1 AND json_extract(meta, '$.providerGroupId') = ?2")?
            .query_map(params![app, group_id], |row| row.get(0))?.collect::<Result<Vec<String>, _>>()?;
        validate_order(expected, ids)?;
        for (index, id) in ids.iter().enumerate() {
            tx.execute("UPDATE providers SET meta = json_set(meta, '$.providerGroupSortIndex', ?1) WHERE id = ?2 AND app_type = ?3",
                params![index, id, app])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_provider_group_members(
        &self,
        app_type: &str,
        group_id: &str,
    ) -> Result<Vec<Provider>, AppError> {
        let providers = self.get_all_providers(app_type)?;
        Ok(providers
            .into_values()
            .filter(|provider| {
                provider
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.provider_group_id.as_deref())
                    == Some(group_id)
            })
            .collect())
    }

    pub fn set_provider_auto_grouping_enabled(
        &self,
        app_type: &str,
        enabled: bool,
    ) -> Result<(), AppError> {
        let key = format!("provider_auto_grouping_{app_type}");
        self.set_setting(&key, if enabled { "true" } else { "false" })
    }

    pub fn provider_auto_grouping_enabled(&self, app_type: &str) -> Result<bool, AppError> {
        let key = format!("provider_auto_grouping_{app_type}");
        self.get_setting(&key)
            .map(|value| matches!(value.as_deref(), Some("true") | Some("1")))
    }

    pub fn set_provider_balance_template(
        &self,
        app_type: &str,
        provider_id: &str,
        template_id: Option<&str>,
    ) -> Result<(), AppError> {
        let mut conn = lock_conn!(self.conn);
        let tx = conn.transaction()?;
        if let Some(template_id) = template_id {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM balance_query_templates WHERE id = ?1)",
                [template_id],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(AppError::InvalidInput(
                    "[balance_template_missing] Balance template no longer exists".into(),
                ));
            }
        }
        let changed = tx.execute(
            "UPDATE providers SET meta = CASE WHEN ?1 IS NULL
             THEN json_remove(meta, '$.balanceTemplateId')
             ELSE json_set(meta, '$.balanceTemplateId', ?1) END
             WHERE id = ?2 AND app_type = ?3",
            params![template_id, provider_id, app_type],
        )?;
        if changed != 1 {
            return Err(AppError::InvalidInput(
                "[balance_provider_missing] Provider does not exist".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }

    pub fn save_balance_query_template(
        &self,
        template: &BalanceQueryTemplate,
    ) -> Result<(), AppError> {
        template.validate()?;
        let balance_scope = match template.balance_scope {
            crate::provider_groups::BalanceScope::Unknown => "unknown",
            crate::provider_groups::BalanceScope::PerKey => "per_key",
            crate::provider_groups::BalanceScope::Account => "account",
        };
        let query = serde_json::to_string(&template.query).map_err(|error| {
            AppError::Database(format!("Serialize balance query params: {error}"))
        })?;
        let headers = serde_json::to_string(&template.headers)
            .map_err(|error| AppError::Database(format!("Serialize balance headers: {error}")))?;
        let now = template.updated_at.max(template.created_at);
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT INTO balance_query_templates (
                id, name, method, path, query_json, headers_json, body,
                remaining_path, used_path, total_path, reset_path, error_path,
                unit, currency, timeout_secs, created_at, updated_at, balance_scope
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
             ON CONFLICT(id) DO UPDATE SET
                name=excluded.name, method=excluded.method, path=excluded.path,
                query_json=excluded.query_json, headers_json=excluded.headers_json,
                body=excluded.body, remaining_path=excluded.remaining_path,
                used_path=excluded.used_path, total_path=excluded.total_path,
                reset_path=excluded.reset_path, error_path=excluded.error_path,
                unit=excluded.unit, currency=excluded.currency,
                timeout_secs=excluded.timeout_secs, updated_at=excluded.updated_at,
                balance_scope=excluded.balance_scope",
            params![
                template.id,
                template.name.trim(),
                template.method,
                template.path,
                query,
                headers,
                template.body,
                template.remaining_path,
                template.used_path,
                template.total_path,
                template.reset_path,
                template.error_path,
                template.unit,
                template.currency,
                template.timeout_secs,
                template.created_at,
                now,
                balance_scope,
            ],
        )?;
        Ok(())
    }

    pub fn get_balance_query_template(
        &self,
        id: &str,
    ) -> Result<Option<BalanceQueryTemplate>, AppError> {
        let conn = lock_conn!(self.conn);
        let row = conn
            .query_row(
                "SELECT id, name, method, path, query_json, headers_json, body,
                        remaining_path, used_path, total_path, reset_path, error_path,
                        unit, currency, timeout_secs, created_at, updated_at, balance_scope
                 FROM balance_query_templates WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, Option<String>>(10)?,
                        row.get::<_, Option<String>>(11)?,
                        row.get::<_, Option<String>>(12)?,
                        row.get::<_, Option<String>>(13)?,
                        row.get::<_, u64>(14)?,
                        row.get::<_, i64>(15)?,
                        row.get::<_, i64>(16)?,
                        row.get::<_, String>(17)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(
                id,
                name,
                method,
                path,
                query_json,
                headers_json,
                body,
                remaining_path,
                used_path,
                total_path,
                reset_path,
                error_path,
                unit,
                currency,
                timeout_secs,
                created_at,
                updated_at,
                balance_scope,
            )| {
                let query = serde_json::from_str(&query_json).map_err(|error| {
                    AppError::Database(format!("Parse balance query params: {error}"))
                })?;
                let headers = serde_json::from_str(&headers_json).map_err(|error| {
                    AppError::Database(format!("Parse balance query headers: {error}"))
                })?;
                Ok(BalanceQueryTemplate {
                    id,
                    name,
                    method,
                    path,
                    query,
                    headers,
                    body,
                    remaining_path,
                    used_path,
                    total_path,
                    reset_path,
                    error_path,
                    unit,
                    currency,
                    balance_scope: serde_json::from_value(serde_json::Value::String(balance_scope))
                        .map_err(|_| AppError::Database("Invalid balance scope".into()))?,
                    timeout_secs,
                    created_at,
                    updated_at,
                })
            },
        )
        .transpose()
    }

    pub fn list_balance_query_templates(&self) -> Result<Vec<BalanceQueryTemplate>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut statement = conn.prepare(
            "SELECT id FROM balance_query_templates ORDER BY name COLLATE NOCASE ASC, id ASC",
        )?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        drop(conn);
        ids.into_iter()
            .map(|id| {
                self.get_balance_query_template(&id)?.ok_or_else(|| {
                    AppError::Database(format!("Balance query template '{id}' disappeared"))
                })
            })
            .collect()
    }

    pub fn delete_balance_query_template(&self, id: &str) -> Result<bool, AppError> {
        let conn = lock_conn!(self.conn);
        Ok(conn.execute(
            "DELETE FROM balance_query_templates WHERE id = ?1",
            params![id],
        )? == 1)
    }
}

#[cfg(test)]
mod tests {
    use crate::database::Database;
    use crate::provider::Provider;
    use crate::provider_groups::{
        BalanceQueryTemplate, KeyPoolStrategy, ProviderGroup, ProviderGroupKind,
    };
    use serde_json::json;

    fn manual_group(app_type: &str, name: &str) -> ProviderGroup {
        ProviderGroup {
            id: format!("group-{app_type}-{name}"),
            app_type: app_type.to_string(),
            name: name.to_string(),
            icon: None,
            icon_color: None,
            kind: ProviderGroupKind::Manual,
            normalized_base_url: None,
            sort_index: 0,
            collapsed: false,
            key_pool_enabled: false,
            key_pool_strategy: KeyPoolStrategy::Failover,
            key_pool_max_retries: 0,
            key_pool_cooldown_ms: 0,
            balance_template_id: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn deleting_group_unassigns_members_without_deleting_providers() {
        let db = Database::memory().expect("memory database");
        let provider = Provider::with_id("p1".into(), "Relay".into(), json!({}), None);
        db.save_provider("codex", &provider).expect("save provider");
        let group = db
            .create_provider_group(&manual_group("codex", "Relay"))
            .expect("create group");
        db.assign_provider_group(
            "codex",
            "p1",
            Some(&group.id),
            Some(0),
            Some(false),
            Some(true),
        )
        .expect("assign provider");

        db.delete_provider_group(&group.id).expect("delete group");

        assert!(db
            .get_provider_by_id("p1", "codex")
            .expect("read provider")
            .is_some());
        assert!(db
            .provider_group_id("codex", "p1")
            .expect("read group assignment")
            .is_none());
        assert!(db
            .list_provider_groups("codex")
            .expect("list groups")
            .is_empty());
    }

    #[test]
    fn balance_query_template_round_trips() {
        let db = Database::memory().expect("memory database");
        let template = BalanceQueryTemplate {
            id: "balance-relay".to_string(),
            name: "Relay balance".to_string(),
            method: "GET".to_string(),
            path: "/user/balance".to_string(),
            query: Default::default(),
            headers: [("Authorization".to_string(), "Bearer {{apiKey}}".to_string())]
                .into_iter()
                .collect(),
            body: None,
            remaining_path: "/data/balance".to_string(),
            used_path: None,
            total_path: Some("/data/total".to_string()),
            reset_path: None,
            error_path: Some("/message".to_string()),
            unit: Some("USD".to_string()),
            currency: Some("USD".to_string()),
            balance_scope: crate::provider_groups::BalanceScope::PerKey,
            timeout_secs: 10,
            created_at: 1,
            updated_at: 1,
        };

        db.save_balance_query_template(&template)
            .expect("save template");
        let loaded = db
            .get_balance_query_template(&template.id)
            .expect("read template")
            .expect("template exists");

        assert_eq!(loaded.id, template.id);
        assert_eq!(loaded.path, "/user/balance");
        assert_eq!(
            loaded.headers.get("Authorization"),
            template.headers.get("Authorization")
        );
        assert_eq!(loaded.total_path.as_deref(), Some("/data/total"));
        assert_eq!(
            loaded.balance_scope,
            crate::provider_groups::BalanceScope::PerKey
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(db.list_balance_query_templates());
        });
        let listed = receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("template listing must not deadlock")
            .unwrap();
        assert_eq!(listed.len(), 1);
    }

    #[test]
    fn folder_appearance_round_trips_without_affecting_legacy_groups() {
        let db = Database::memory().unwrap();
        let legacy = manual_group("codex", "Custom");
        let mut value = serde_json::to_value(&legacy).unwrap();
        value["icon"] = json!("star");
        value["iconColor"] = json!("#22c55e");
        let group = serde_json::from_value(value).unwrap();
        db.create_provider_group(&group).unwrap();
        let loaded = db.get_provider_group(&legacy.id).unwrap().unwrap();
        let loaded = serde_json::to_value(loaded).unwrap();
        assert_eq!(loaded["icon"], "star");
        assert_eq!(loaded["iconColor"], "#22c55e");
    }

    #[test]
    fn folder_order_is_atomic_and_scoped_to_the_app() {
        let db = Database::memory().unwrap();
        for (app, name) in [("codex", "A"), ("codex", "B"), ("claude", "C")] {
            db.create_provider_group(&manual_group(app, name)).unwrap();
        }
        let order = vec!["group-codex-B".into(), "group-codex-A".into()];
        db.reorder_provider_groups("codex", &order).unwrap();
        assert!(db
            .reorder_provider_groups("codex", &["group-codex-A".into(), "group-claude-C".into()])
            .is_err());
        assert!(db
            .reorder_provider_groups("codex", &["group-codex-A".into(), "group-codex-A".into()])
            .is_err());
        assert_eq!(
            db.list_provider_groups("codex")
                .unwrap()
                .into_iter()
                .map(|g| g.id)
                .collect::<Vec<_>>(),
            order
        );
        assert_eq!(db.list_provider_groups("claude").unwrap()[0].sort_index, 0);
    }

    #[test]
    fn folder_membership_changes_preserve_unknown_metadata_and_order() {
        let db = Database::memory().unwrap();
        let group = db
            .create_provider_group(&manual_group("claude", "Pool"))
            .unwrap();
        for id in ["one", "two"] {
            let provider = Provider::with_id(
                id.into(),
                id.into(),
                json!({"env":{"ANTHROPIC_BASE_URL":"https://relay.example", "ANTHROPIC_API_KEY":"fixture-key"}}),
                None,
            );
            db.save_provider("claude", &provider).unwrap();
            db.conn.lock().unwrap().execute("UPDATE providers SET meta = '{\"futureField\":{\"keep\":true}}' WHERE id = ?1 AND app_type = 'claude'", [id]).unwrap();
            db.assign_provider_group("claude", id, Some(&group.id), None, Some(false), Some(true))
                .unwrap();
        }
        db.reorder_provider_group_members(&group.id, &["two".into(), "one".into()])
            .unwrap();
        db.assign_provider_group("claude", "two", Some(&group.id), None, Some(true), None)
            .unwrap();
        assert_eq!(
            db.get_provider_by_id("two", "claude")
                .unwrap()
                .unwrap()
                .meta
                .unwrap()
                .provider_group_sort_index,
            Some(0)
        );
        assert!(db
            .reorder_provider_group_members(&group.id, &["one".into(), "one".into()])
            .is_err());
        db.delete_provider_group(&group.id).unwrap();
        let raw: String = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT meta FROM providers WHERE id = 'two' AND app_type = 'claude'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let meta: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(meta["futureField"]["keep"], true);
        assert_eq!(meta["providerGroupManual"], true);
        assert!(meta.get("providerGroupId").is_none());
    }
}
