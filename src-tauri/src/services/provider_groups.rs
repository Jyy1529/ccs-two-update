//! Provider grouping, automatic Base URL grouping, and key-pool policy checks.

use crate::app_config::AppType;
use crate::database::Database;
use crate::error::AppError;
use crate::provider::Provider;
use crate::provider_groups::{
    normalize_base_url, KeyPoolStrategy, ProviderGroup, ProviderGroupKind,
    ProviderGroupMemberStatus, ProviderGroupStatus,
};
use std::collections::BTreeMap;
use std::str::FromStr;
use uuid::Uuid;

pub struct ProviderGroupService;

impl ProviderGroupService {
    fn app_type(app_type: &str) -> Result<AppType, AppError> {
        AppType::from_str(app_type)
    }

    fn validate_group_app(group: &ProviderGroup) -> Result<AppType, AppError> {
        let app = Self::app_type(&group.app_type)?;
        if group.app_type != app.as_str() {
            return Err(AppError::InvalidInput(format!(
                "Provider group app type must use canonical name: {}",
                app.as_str()
            )));
        }
        Ok(app)
    }

    fn next_group_index(db: &Database, app_type: &str) -> Result<usize, AppError> {
        Ok(db
            .list_provider_groups(app_type)?
            .into_iter()
            .map(|group| group.sort_index)
            .max()
            .map_or(0, |last| last.saturating_add(1)))
    }

    fn auto_group_name(normalized_base_url: &str) -> String {
        url::Url::parse(normalized_base_url)
            .ok()
            .and_then(|url| url.host_str().map(ToString::to_string))
            .filter(|host| !host.is_empty())
            .unwrap_or_else(|| normalized_base_url.to_string())
    }

    fn new_auto_group(
        db: &Database,
        app_type: &str,
        normalized_base_url: &str,
    ) -> Result<ProviderGroup, AppError> {
        let now = chrono::Utc::now().timestamp_millis();
        let group = ProviderGroup {
            id: format!("provider-group-{}", Uuid::new_v4()),
            app_type: app_type.to_string(),
            name: Self::auto_group_name(normalized_base_url),
            icon: None,
            icon_color: None,
            kind: ProviderGroupKind::AutoBaseUrl,
            normalized_base_url: Some(normalized_base_url.to_string()),
            sort_index: Self::next_group_index(db, app_type)?,
            collapsed: false,
            key_pool_enabled: false,
            key_pool_strategy: KeyPoolStrategy::Failover,
            key_pool_max_retries: 0,
            key_pool_cooldown_ms: 0,
            balance_template_id: None,
            created_at: now,
            updated_at: now,
        };
        db.create_provider_group(&group)
    }

    pub fn list_groups(db: &Database, app_type: &str) -> Result<Vec<ProviderGroup>, AppError> {
        let app = Self::app_type(app_type)?;
        if db.provider_auto_grouping_enabled(app.as_str())? {
            return Self::reconcile_auto_groups(db, app.as_str());
        }
        db.list_provider_groups(app.as_str())
    }

    pub fn create_group(
        db: &Database,
        mut group: ProviderGroup,
    ) -> Result<ProviderGroup, AppError> {
        let app = Self::validate_group_app(&group)?;
        group.app_type = app.as_str().to_string();
        group.name = group.name.trim().to_string();
        if matches!(group.kind, ProviderGroupKind::Manual) {
            group.normalized_base_url = None;
        } else if let Some(url) = group.normalized_base_url.as_deref() {
            group.normalized_base_url = Some(normalize_base_url(url)?);
        }
        if group.id.trim().is_empty() {
            group.id = format!("provider-group-{}", Uuid::new_v4());
        }
        if group.created_at <= 0 {
            group.created_at = chrono::Utc::now().timestamp_millis();
        }
        if group.updated_at <= 0 {
            group.updated_at = group.created_at;
        }
        db.create_provider_group(&group)
    }

    pub fn update_group(db: &Database, mut group: ProviderGroup) -> Result<(), AppError> {
        let app = Self::validate_group_app(&group)?;
        group.app_type = app.as_str().to_string();
        group.name = group.name.trim().to_string();
        if matches!(group.kind, ProviderGroupKind::Manual) {
            group.normalized_base_url = None;
        } else if let Some(url) = group.normalized_base_url.as_deref() {
            group.normalized_base_url = Some(normalize_base_url(url)?);
        }
        group.updated_at = chrono::Utc::now().timestamp_millis();
        if group.key_pool_enabled {
            Self::validate_pool_members_for_group(db, &group)?;
        }
        db.update_provider_group(&group)
    }

    pub fn delete_group(db: &Database, group_id: &str) -> Result<(), AppError> {
        if group_id.trim().is_empty() {
            return Err(AppError::InvalidInput(
                "Provider group ID cannot be empty".to_string(),
            ));
        }
        db.delete_provider_group(group_id)
    }

    pub fn move_provider(
        db: &Database,
        app_type: &str,
        provider_id: &str,
        group_id: Option<&str>,
    ) -> Result<(), AppError> {
        let app = Self::app_type(app_type)?;
        if let Some(group_id) = group_id {
            let group = db.get_provider_group(group_id)?.ok_or_else(|| {
                AppError::InvalidInput("Provider group does not exist".to_string())
            })?;
            if group.app_type != app.as_str() {
                return Err(AppError::InvalidInput(
                    "Provider and group must belong to the same app".to_string(),
                ));
            }
        }
        db.assign_provider_group(app.as_str(), provider_id, group_id, None, None, Some(true))
    }

    pub fn set_provider_key_pool_enabled(
        db: &Database,
        app_type: &str,
        provider_id: &str,
        enabled: bool,
    ) -> Result<(), AppError> {
        let app = Self::app_type(app_type)?;
        let group_id = db
            .provider_group_id(app.as_str(), provider_id)?
            .ok_or_else(|| {
                AppError::InvalidInput("Provider is not assigned to a group".to_string())
            })?;
        let group = db
            .get_provider_group(&group_id)?
            .ok_or_else(|| AppError::Database("Provider group disappeared".to_string()))?;
        if enabled {
            let provider = db
                .get_provider_by_id(provider_id, app.as_str())?
                .ok_or_else(|| AppError::Database("Provider disappeared".to_string()))?;
            Self::validate_provider_pool_eligibility(&app, &provider)?;
        }
        db.assign_provider_group(
            app.as_str(),
            provider_id,
            Some(&group.id),
            None,
            Some(enabled),
            None,
        )
    }

    pub fn update_policy(
        db: &Database,
        group_id: &str,
        enabled: bool,
        strategy: KeyPoolStrategy,
        max_retries: u32,
        cooldown_ms: u64,
    ) -> Result<ProviderGroup, AppError> {
        let mut group = db
            .get_provider_group(group_id)?
            .ok_or_else(|| AppError::InvalidInput("Provider group does not exist".to_string()))?;
        group.key_pool_enabled = enabled;
        group.key_pool_strategy = strategy;
        group.key_pool_max_retries = max_retries;
        group.key_pool_cooldown_ms = cooldown_ms;
        group.updated_at = chrono::Utc::now().timestamp_millis();
        if enabled {
            Self::validate_pool_members_for_group(db, &group)?;
        }
        db.update_provider_group(&group)?;
        Ok(group)
    }

    pub fn set_auto_grouping(
        db: &Database,
        app_type: &str,
        enabled: bool,
    ) -> Result<Vec<ProviderGroup>, AppError> {
        let app = Self::app_type(app_type)?;
        db.set_provider_auto_grouping_enabled(app.as_str(), enabled)?;
        if enabled {
            Self::reconcile_auto_groups(db, app.as_str())
        } else {
            db.list_provider_groups(app.as_str())
        }
    }

    pub fn auto_grouping_enabled(db: &Database, app_type: &str) -> Result<bool, AppError> {
        let app = Self::app_type(app_type)?;
        db.provider_auto_grouping_enabled(app.as_str())
    }

    pub fn reconcile_auto_groups(
        db: &Database,
        app_type: &str,
    ) -> Result<Vec<ProviderGroup>, AppError> {
        let app = Self::app_type(app_type)?;
        if !db.provider_auto_grouping_enabled(app.as_str())? {
            return db.list_provider_groups(app.as_str());
        }

        let providers = db.get_all_providers(app.as_str())?;
        let mut candidates: BTreeMap<String, Vec<(&Provider, Option<ProviderGroup>)>> =
            BTreeMap::new();
        for provider in providers.values() {
            if provider
                .meta
                .as_ref()
                .and_then(|meta| meta.provider_group_manual)
                .unwrap_or(false)
            {
                continue;
            }
            let current_group = if let Some(id) = provider
                .meta
                .as_ref()
                .and_then(|meta| meta.provider_group_id.as_deref())
            {
                db.get_provider_group(id)?
            } else {
                None
            };
            if current_group
                .as_ref()
                .is_some_and(|group| matches!(group.kind, ProviderGroupKind::Manual))
            {
                continue;
            }

            let (base_url, _) = crate::provider_groups::resolve_group_credentials(&app, provider);
            let Ok(normalized) = normalize_base_url(&base_url) else {
                if current_group.is_some() {
                    db.assign_provider_group(
                        app.as_str(),
                        &provider.id,
                        None,
                        None,
                        Some(false),
                        Some(false),
                    )?;
                }
                continue;
            };
            candidates
                .entry(normalized)
                .or_default()
                .push((provider, current_group));
        }

        for (normalized, members) in candidates {
            if members.len() < 2 {
                for (provider, current_group) in members {
                    if current_group.is_some() {
                        db.assign_provider_group(
                            app.as_str(),
                            &provider.id,
                            None,
                            None,
                            Some(false),
                            Some(false),
                        )?;
                    }
                }
                continue;
            }
            let group = match db.find_auto_provider_group(app.as_str(), &normalized)? {
                Some(group) => group,
                None => Self::new_auto_group(db, app.as_str(), &normalized)?,
            };
            for (provider, current_group) in members {
                if current_group.as_ref().map(|group| group.id.as_str()) != Some(group.id.as_str())
                {
                    let next_index = db
                        .get_provider_group_members(app.as_str(), &group.id)?
                        .into_iter()
                        .filter_map(|member| {
                            member.meta.and_then(|meta| meta.provider_group_sort_index)
                        })
                        .max()
                        .map_or(0, |last| last.saturating_add(1));
                    db.assign_provider_group(
                        app.as_str(),
                        &provider.id,
                        Some(&group.id),
                        Some(next_index),
                        Some(false),
                        Some(false),
                    )?;
                }
            }
        }
        for group in db.list_provider_groups(app.as_str())? {
            if matches!(group.kind, ProviderGroupKind::AutoBaseUrl)
                && !group.key_pool_enabled
                && group.balance_template_id.is_none()
                && group.icon.is_none()
                && group.icon_color.is_none()
                && group
                    .normalized_base_url
                    .as_deref()
                    .is_some_and(|base| group.name == Self::auto_group_name(base))
                && db
                    .get_provider_group_members(app.as_str(), &group.id)?
                    .is_empty()
            {
                db.delete_provider_group(&group.id)?;
            }
        }
        db.list_provider_groups(app.as_str())
    }

    pub fn validate_pool_members(db: &Database, group_id: &str) -> Result<Vec<Provider>, AppError> {
        let group = db
            .get_provider_group(group_id)?
            .ok_or_else(|| AppError::InvalidInput("Provider group does not exist".to_string()))?;
        Self::validate_pool_members_for_group(db, &group)
    }

    /// Return a redacted status snapshot for the UI. Eligibility errors are
    /// returned as short messages and never include resolved credentials.
    pub fn group_status(db: &Database, group_id: &str) -> Result<ProviderGroupStatus, AppError> {
        let group = db
            .get_provider_group(group_id)?
            .ok_or_else(|| AppError::InvalidInput("Provider group does not exist".to_string()))?;
        let app = Self::app_type(&group.app_type)?;
        let mut members = db.get_provider_group_members(app.as_str(), &group.id)?;
        members.sort_by_key(|provider| {
            (
                provider
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.provider_group_sort_index)
                    .unwrap_or(usize::MAX),
                provider.sort_index.unwrap_or(usize::MAX),
                provider.id.clone(),
            )
        });
        let expected = members
            .iter()
            .filter(|provider| {
                provider
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.key_pool_enabled)
                    .unwrap_or(false)
            })
            .find_map(|provider| crate::provider_groups::key_pool_identity(&app, provider).ok());
        let mut statuses = Vec::with_capacity(members.len());
        let mut eligible_member_count = 0;

        for provider in members {
            let key_pool_enabled = provider
                .meta
                .as_ref()
                .and_then(|meta| meta.key_pool_enabled)
                .unwrap_or(false);
            let error = match crate::provider_groups::key_pool_identity(&app, &provider) {
                Err(reason) => Some(reason.to_string()),
                Ok(identity) if expected.as_ref().is_some_and(|expected| expected != &identity)
                    || group.normalized_base_url.as_ref().is_some_and(|base| base != &identity.0) =>
                    Some("[pool_base_url_mismatch] Key pool members must have the same normalized Base URL and protocol".into()),
                Ok(_) => None,
            };
            let eligible = group.key_pool_enabled && key_pool_enabled && error.is_none();
            if eligible {
                eligible_member_count += 1;
            }
            statuses.push(ProviderGroupMemberStatus {
                provider_id: provider.id,
                provider_name: provider.name,
                sort_index: provider
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.provider_group_sort_index),
                key_pool_enabled,
                eligible,
                cooling_down: false,
                cooldown_remaining_ms: 0,
                consecutive_failures: 0,
                last_failure_at: None,
                early_probe: false,
                error,
            });
        }

        Ok(ProviderGroupStatus {
            group,
            members: statuses,
            eligible_member_count,
            proxy_running: false,
        })
    }

    fn validate_pool_members_for_group(
        db: &Database,
        group: &ProviderGroup,
    ) -> Result<Vec<Provider>, AppError> {
        let app = Self::app_type(&group.app_type)?;
        let members = db.get_provider_group_members(app.as_str(), &group.id)?;
        let eligible: Vec<_> = members
            .into_iter()
            .filter(|provider| {
                provider
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.key_pool_enabled)
                    .unwrap_or(false)
            })
            .collect();
        crate::provider_groups::validate_key_pool_members(
            &app,
            group.normalized_base_url.as_deref(),
            &eligible,
        )?;
        if group.key_pool_enabled && eligible.is_empty() {
            return Err(AppError::InvalidInput(
                "[pool_empty] Enable at least one eligible Key member first".to_string(),
            ));
        }
        Ok(eligible)
    }

    fn validate_provider_pool_eligibility(
        app_type: &AppType,
        provider: &Provider,
    ) -> Result<(), AppError> {
        crate::provider_groups::key_pool_identity(app_type, provider).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::provider::Provider;
    use crate::provider_groups::{KeyPoolStrategy, ProviderGroup, ProviderGroupKind};
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
    fn auto_grouping_does_not_override_manual_group_membership() {
        let db = Database::memory().expect("memory database");
        let manual = db
            .create_provider_group(&manual_group("codex", "Pinned"))
            .expect("create manual group");
        let provider = Provider::with_id(
            "p1".to_string(),
            "Relay".to_string(),
            json!({
                "auth": {"OPENAI_API_KEY": "key-1"},
                "config": "model_provider = \"relay\"\n[model_providers.relay]\nbase_url = \"https://relay.example/v1\"\n"
            }),
            None,
        );
        db.save_provider("codex", &provider).expect("save provider");
        db.assign_provider_group(
            "codex",
            "p1",
            Some(&manual.id),
            Some(0),
            Some(false),
            Some(true),
        )
        .expect("assign provider");

        ProviderGroupService::set_auto_grouping(&db, "codex", true).expect("reconcile groups");

        assert_eq!(
            db.provider_group_id("codex", "p1")
                .expect("read group assignment")
                .as_deref(),
            Some(manual.id.as_str())
        );
    }

    #[test]
    fn first_auto_group_can_be_created_and_manual_ungrouping_is_preserved() {
        let db = Database::memory().unwrap();
        let provider = Provider::with_id(
            "p1".into(),
            "Relay".into(),
            json!({
                "env": {"ANTHROPIC_BASE_URL": "https://relay.example/v1", "ANTHROPIC_API_KEY": "test-key"}
            }),
            None,
        );
        db.save_provider("claude", &provider).unwrap();
        let mut second = provider.clone();
        second.id = "p2".into();
        db.save_provider("claude", &second).unwrap();
        let groups = ProviderGroupService::set_auto_grouping(&db, "claude", true).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].sort_index, 0);
        ProviderGroupService::move_provider(&db, "claude", "p1", None).unwrap();
        ProviderGroupService::list_groups(&db, "claude").unwrap();
        assert_eq!(db.provider_group_id("claude", "p1").unwrap(), None);
        assert_eq!(db.provider_group_id("claude", "p2").unwrap(), None);
        assert!(ProviderGroupService::list_groups(&db, "claude")
            .unwrap()
            .is_empty());
        second.id = "p3".into();
        db.save_provider("claude", &second).unwrap();
        ProviderGroupService::list_groups(&db, "claude").unwrap();
        assert_eq!(db.provider_group_id("claude", "p1").unwrap(), None);
        assert!(db.provider_group_id("claude", "p2").unwrap().is_some());
    }

    #[test]
    fn pool_members_reject_mismatched_urls_and_provider_edits() {
        let db = Database::memory().unwrap();
        let group = db
            .create_provider_group(&manual_group("claude", "Pool"))
            .unwrap();
        for (id, url) in [
            ("p1", "https://one.example/v1"),
            ("p2", "https://two.example/v1"),
        ] {
            let provider = Provider::with_id(
                id.into(),
                id.into(),
                json!({
                    "env": {"ANTHROPIC_BASE_URL": url, "ANTHROPIC_API_KEY": "test-key"}
                }),
                None,
            );
            db.save_provider("claude", &provider).unwrap();
            ProviderGroupService::move_provider(&db, "claude", id, Some(&group.id)).unwrap();
        }
        ProviderGroupService::set_provider_key_pool_enabled(&db, "claude", "p1", true).unwrap();
        assert!(
            ProviderGroupService::set_provider_key_pool_enabled(&db, "claude", "p2", true).is_err()
        );
        let mut p2 = db.get_provider_by_id("p2", "claude").unwrap().unwrap();
        p2.settings_config["env"]["ANTHROPIC_BASE_URL"] = json!("https://one.example/v1/");
        db.save_provider("claude", &p2).unwrap();
        ProviderGroupService::set_provider_key_pool_enabled(&db, "claude", "p2", true).unwrap();
        let mut p2 = db.get_provider_by_id("p2", "claude").unwrap().unwrap();
        p2.settings_config["env"]["ANTHROPIC_BASE_URL"] = json!("https://two.example/v1");
        assert!(db.save_provider("claude", &p2).is_err());
        assert_eq!(
            db.get_provider_by_id("p2", "claude")
                .unwrap()
                .unwrap()
                .settings_config["env"]["ANTHROPIC_BASE_URL"],
            "https://one.example/v1/"
        );
    }

    #[test]
    fn pool_writes_reject_cross_app_managed_auth_and_protocol_changes_atomically() {
        let db = Database::memory().unwrap();
        let group = db
            .create_provider_group(&manual_group("claude", "Pool"))
            .unwrap();
        let mut p1 = Provider::with_id(
            "p1".into(),
            "p1".into(),
            json!({"env": {
                "ANTHROPIC_BASE_URL":"https://relay.example/v1", "ANTHROPIC_API_KEY":"fixture-key"
            }}),
            None,
        );
        p1.meta = Some(crate::provider::ProviderMeta {
            provider_group_id: Some(group.id.clone()),
            key_pool_enabled: Some(true),
            ..Default::default()
        });
        db.save_provider("claude", &p1).unwrap();
        let mut p2 = p1.clone();
        p2.id = "p2".into();
        p2.meta.as_mut().unwrap().api_format = Some("openai_chat".into());
        assert!(db
            .save_provider("claude", &p2)
            .unwrap_err()
            .to_string()
            .contains("pool_base_url_mismatch"));
        p2.meta.as_mut().unwrap().api_format = None;
        p2.meta.as_mut().unwrap().provider_type = Some("github_copilot".into());
        assert!(db.save_provider("claude", &p2).is_err());
        p2.meta.as_mut().unwrap().provider_type = None;
        p2.meta.as_mut().unwrap().key_pool_enabled = Some(false);
        assert!(db.save_provider("codex", &p2).is_err());
        assert!(db.get_provider_by_id("p2", "claude").unwrap().is_none());
        assert!(db.get_provider_by_id("p2", "codex").unwrap().is_none());
        let other_group = db
            .create_provider_group(&manual_group("codex", "Other"))
            .unwrap();
        assert!(
            ProviderGroupService::move_provider(&db, "claude", "p1", Some(&other_group.id))
                .is_err()
        );
        assert_eq!(
            db.provider_group_id("claude", "p1").unwrap().as_deref(),
            Some(group.id.as_str())
        );
        let empty = db
            .create_provider_group(&manual_group("claude", "Empty"))
            .unwrap();
        assert!(ProviderGroupService::update_policy(
            &db,
            &empty.id,
            true,
            KeyPoolStrategy::Failover,
            0,
            0
        )
        .is_err());
        assert!(
            !db.get_provider_group(&empty.id)
                .unwrap()
                .unwrap()
                .key_pool_enabled
        );
    }

    #[test]
    fn auto_grouping_switches_are_independent_and_invalid_urls_do_not_block_other_cards() {
        let db = Database::memory().unwrap();
        let valid = Provider::with_id(
            "valid".into(),
            "valid".into(),
            json!({"env":{"ANTHROPIC_BASE_URL":"https://relay.example","ANTHROPIC_API_KEY":"fixture-key"}}),
            None,
        );
        let invalid = Provider::with_id(
            "invalid".into(),
            "invalid".into(),
            json!({"env":{"ANTHROPIC_BASE_URL":"not a URL"}}),
            None,
        );
        db.save_provider("claude", &valid).unwrap();
        let mut matching = valid.clone();
        matching.id = "matching".into();
        db.save_provider("claude", &matching).unwrap();
        db.save_provider("claude", &invalid).unwrap();
        ProviderGroupService::set_auto_grouping(&db, "claude", true).unwrap();
        assert!(ProviderGroupService::auto_grouping_enabled(&db, "claude").unwrap());
        assert!(!ProviderGroupService::auto_grouping_enabled(&db, "codex").unwrap());
        assert!(db.provider_group_id("claude", "valid").unwrap().is_some());
        assert!(db.provider_group_id("claude", "invalid").unwrap().is_none());
    }

    #[test]
    fn auto_grouping_requires_two_matching_normalized_base_urls() {
        let db = Database::memory().unwrap();
        for (id, base) in [
            ("one", "https://one.example/v1"),
            ("two", "https://two.example/v1"),
        ] {
            let provider = Provider::with_id(
                id.into(),
                id.into(),
                json!({
                    "env": {"ANTHROPIC_BASE_URL": base, "ANTHROPIC_API_KEY": "fixture-key"}
                }),
                None,
            );
            db.save_provider("claude", &provider).unwrap();
        }
        assert!(ProviderGroupService::set_auto_grouping(&db, "claude", true)
            .unwrap()
            .is_empty());
        assert_eq!(db.provider_group_id("claude", "one").unwrap(), None);
        assert_eq!(db.provider_group_id("claude", "two").unwrap(), None);
        let matching = Provider::with_id(
            "matching".into(),
            "Matching".into(),
            json!({
                "env": {"ANTHROPIC_BASE_URL": "https://ONE.example/v1/", "ANTHROPIC_API_KEY": "another-fixture-key"}
            }),
            None,
        );
        db.save_provider("claude", &matching).unwrap();
        let groups = ProviderGroupService::list_groups(&db, "claude").unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(
            db.get_provider_group_members("claude", &groups[0].id)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(db.provider_group_id("claude", "two").unwrap(), None);
    }

    #[test]
    fn auto_grouping_returns_singletons_to_ungrouped_after_base_changes_or_deletion() {
        let db = Database::memory().unwrap();
        let mut first = Provider::with_id(
            "one".into(),
            "One".into(),
            json!({
                "env": {"ANTHROPIC_BASE_URL": "https://one.example/v1", "ANTHROPIC_API_KEY": "fixture-key"}
            }),
            None,
        );
        let mut second = first.clone();
        second.id = "two".into();
        db.save_provider("claude", &first).unwrap();
        db.save_provider("claude", &second).unwrap();
        assert_eq!(
            ProviderGroupService::set_auto_grouping(&db, "claude", true)
                .unwrap()
                .len(),
            1
        );
        second.settings_config["env"]["ANTHROPIC_BASE_URL"] = json!("https://two.example/v1");
        db.save_provider("claude", &second).unwrap();
        assert!(ProviderGroupService::list_groups(&db, "claude")
            .unwrap()
            .is_empty());
        assert_eq!(db.provider_group_id("claude", "one").unwrap(), None);
        assert_eq!(db.provider_group_id("claude", "two").unwrap(), None);
        first.settings_config["env"]["ANTHROPIC_BASE_URL"] = json!("https://two.example/v1");
        db.save_provider("claude", &first).unwrap();
        assert_eq!(
            ProviderGroupService::list_groups(&db, "claude")
                .unwrap()
                .len(),
            1
        );
        db.delete_provider("claude", "two").unwrap();
        assert!(ProviderGroupService::list_groups(&db, "claude")
            .unwrap()
            .is_empty());
        assert_eq!(db.provider_group_id("claude", "one").unwrap(), None);
    }
}
