//! Shared single-provider and folder balance dispatch. Credentials never leave this boundary.
#[cfg(test)]
mod tests;

use crate::{
    app_config::AppType,
    database::Database,
    error::AppError,
    provider::{Provider, UsageResult},
    provider_groups::{normalize_base_url, BalanceQueryResult, BalanceQueryTemplate, BalanceScope},
};
use futures::{stream, StreamExt};
use std::collections::{HashMap, HashSet};

fn group_template(
    db: &Database,
    template_id: Option<&str>,
) -> Result<Option<BalanceQueryTemplate>, AppError> {
    template_id
        .map(|id| {
            db.get_balance_query_template(id)?.ok_or_else(|| {
                AppError::InvalidInput(
                    "[balance_template_missing] Balance template no longer exists".into(),
                )
            })
        })
        .transpose()
}

async fn query_member(
    db: &Database,
    app: &AppType,
    provider: &Provider,
    group_template_id: Option<&str>,
) -> BalanceQueryResult {
    let template_id = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.balance_template_id.as_deref())
        .or(group_template_id);
    let template = match group_template(db, template_id) {
        Ok(template) => template,
        Err(error) => {
            return BalanceQueryResult {
                provider_id: provider.id.clone(),
                provider_name: provider.name.clone(),
                status: "failed".into(),
                data: Vec::new(),
                error: Some(error.to_string()),
                currency: None,
                aggregation_key: None,
            };
        }
    };
    let template = template.as_ref();
    let (base, key) = crate::provider_groups::resolve_group_credentials(app, provider);
    let outcome = if key.trim().is_empty() || base.trim().is_empty() {
        Err("[balance_credentials] Base URL and static API Key are required".to_string())
    } else if let Some(template) = template {
        super::balance_query::query_balance_template(template, &base, &key)
            .await
            .map_err(|error| error.to_string())
    } else {
        super::balance::get_balance(&base, &key)
            .await
            .map_err(|_| "[balance_network] Built-in balance request failed".to_string())
    };
    let result = outcome.unwrap_or_else(|error| UsageResult {
        success: false,
        data: None,
        error: Some(error),
    });
    let mut result = super::balance_query::redact_balance_result(result, &key);
    if template.is_none() && result.error.as_deref() == Some("Unknown balance provider") {
        result.error = Some(
            "[balance_builtin_unknown] No built-in balance endpoint; select a custom template"
                .into(),
        );
    }
    BalanceQueryResult {
        provider_id: provider.id.clone(),
        provider_name: provider.name.clone(),
        status: if result.success { "success" } else { "failed" }.into(),
        data: result.data.unwrap_or_default(),
        error: result.error,
        currency: template.and_then(|template| template.currency.clone()),
        aggregation_key: template
            .filter(|template| result.success && template.balance_scope == BalanceScope::PerKey)
            .map(|template| format!("template:{}:{}", template.id, template.remaining_path)),
    }
}

pub async fn query_provider(
    db: &Database,
    app: &AppType,
    provider_id: &str,
) -> Result<BalanceQueryResult, AppError> {
    let provider = db
        .get_provider_by_id(provider_id, app.as_str())?
        .ok_or_else(|| {
            AppError::InvalidInput("[balance_provider_missing] Provider does not exist".into())
        })?;
    let group = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.provider_group_id.as_deref())
        .map(|id| db.get_provider_group(id))
        .transpose()?
        .flatten();
    let duplicate = if let Some(group) = group.as_ref() {
        duplicate_pool_members(
            app,
            &db.get_provider_group_members(app.as_str(), &group.id)?,
        )
        .contains(provider_id)
    } else {
        false
    };
    let mut result = query_member(
        db,
        app,
        &provider,
        group
            .as_ref()
            .and_then(|group| group.balance_template_id.as_deref()),
    )
    .await;
    if duplicate {
        result.aggregation_key = None;
    }
    Ok(result)
}

fn duplicate_pool_members(app: &AppType, members: &[Provider]) -> HashSet<String> {
    let mut duplicated = HashSet::new();
    // Do not export a credential or its hash as an aggregation identity.
    let mut seen = HashMap::new();
    for provider in members.iter().filter(|p| {
        p.meta
            .as_ref()
            .and_then(|m| m.key_pool_enabled)
            .unwrap_or(false)
    }) {
        let (base, key) = crate::provider_groups::resolve_group_credentials(app, provider);
        if let Ok(base) = normalize_base_url(&base) {
            if let Some(previous) = seen.insert((base, key.trim().to_string()), provider.id.clone())
            {
                duplicated.insert(previous);
                duplicated.insert(provider.id.clone());
            }
        }
    }
    duplicated
}

pub async fn query_group(
    db: &Database,
    group_id: &str,
) -> Result<Vec<BalanceQueryResult>, AppError> {
    let group = db.get_provider_group(group_id)?.ok_or_else(|| {
        AppError::InvalidInput("[balance_group_missing] Folder does not exist".into())
    })?;
    let app: AppType = group.app_type.parse()?;
    let mut members = db.get_provider_group_members(app.as_str(), group_id)?;
    members.sort_by_key(|p| {
        (
            p.meta
                .as_ref()
                .and_then(|m| m.provider_group_sort_index)
                .unwrap_or(usize::MAX),
            p.sort_index.unwrap_or(usize::MAX),
            p.id.clone(),
        )
    });
    let duplicated = duplicate_pool_members(&app, &members);
    let mut results = stream::iter(members.into_iter().enumerate().map(|(index, member)| {
        let app = app.clone();
        let template_id = group.balance_template_id.clone();
        async move {
            (
                index,
                query_member(db, &app, &member, template_id.as_deref()).await,
            )
        }
    }))
    .buffer_unordered(4)
    .collect::<Vec<_>>()
    .await;
    results.sort_by_key(|(index, _)| *index);
    Ok(results
        .into_iter()
        .map(|(_, mut result)| {
            if duplicated.contains(&result.provider_id) {
                result.aggregation_key = None;
            }
            result
        })
        .collect())
}
