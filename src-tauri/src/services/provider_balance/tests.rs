use super::*;
use crate::provider::ProviderMeta;
use crate::provider_groups::ProviderGroup;
use axum::{
    http::{HeaderMap, StatusCode},
    routing::get,
    Json, Router,
};
use serde_json::json;

fn folder() -> ProviderGroup {
    serde_json::from_value(json!({
        "id":"folder", "appType":"claude", "name":"Test folder", "kind":"manual",
        "normalizedBaseUrl":null, "sortIndex":0, "collapsed":false,
        "keyPoolEnabled":false, "keyPoolStrategy":"failover", "keyPoolMaxRetries":0,
        "keyPoolCooldownMs":1000, "balanceTemplateId":null, "createdAt":1, "updatedAt":1
    }))
    .unwrap()
}

fn template() -> BalanceQueryTemplate {
    serde_json::from_value(json!({
        "id":"template", "name":"Test balance", "method":"GET", "path":"/balance",
        "headers":{"Authorization":"Bearer {{apiKey}}"}, "remainingPath":"/balance",
        "unit":"USD", "currency":"USD", "balanceScope":"per_key", "timeoutSecs":2,
        "createdAt":1, "updatedAt":1
    }))
    .unwrap()
}

fn provider(id: &str, base: &str, key: &str) -> Provider {
    Provider::with_id(
        id.into(),
        id.into(),
        json!({"env":{"ANTHROPIC_BASE_URL":base,"ANTHROPIC_AUTH_TOKEN":key}}),
        None,
    )
}

#[tokio::test]
async fn independent_provider_and_folder_without_template_use_builtin_dispatch() {
    let db = Database::memory().unwrap();
    let p = provider("one", "https://relay.example/v1", "fixture-key");
    db.save_provider("claude", &p).unwrap();
    let independent = query_provider(&db, &AppType::Claude, &p.id).await.unwrap();
    assert!(independent
        .error
        .unwrap()
        .contains("balance_builtin_unknown"));
    let group = folder();
    db.create_provider_group(&group).unwrap();
    db.assign_provider_group(
        "claude",
        &p.id,
        Some(&group.id),
        None,
        Some(false),
        Some(true),
    )
    .unwrap();
    let results = query_group(&db, &group.id).await.unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0]
        .error
        .as_ref()
        .unwrap()
        .contains("balance_builtin_unknown"));
    assert!(results[0].aggregation_key.is_none());
    db.delete_provider_group(&group.id).unwrap();
    assert!(db.get_provider_by_id(&p.id, "claude").unwrap().is_some());
}

#[tokio::test]
async fn custom_template_queries_keep_partial_results_and_exclude_duplicate_or_account_balances() {
    let app = Router::new().route(
        "/balance",
        get(|headers: HeaderMap| async move {
            if headers["authorization"] == "Bearer fixture-bad" {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error":"fixture-bad"})),
                )
            } else {
                (StatusCode::OK, Json(json!({"balance":12.5})))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let db = Database::memory().unwrap();
    let mut template = template();
    db.save_balance_query_template(&template).unwrap();
    let mut group = folder();
    group.balance_template_id = Some(template.id.clone());
    db.create_provider_group(&group).unwrap();
    for (index, key) in ["fixture-one", "fixture-two", "fixture-one", "fixture-bad"]
        .iter()
        .enumerate()
    {
        let mut p = provider(&format!("p{index}"), &base, key);
        p.meta = Some(ProviderMeta {
            provider_group_id: Some(group.id.clone()),
            provider_group_sort_index: Some(index),
            key_pool_enabled: Some(true),
            ..Default::default()
        });
        db.save_provider("claude", &p).unwrap();
    }
    let results = query_group(&db, &group.id).await.unwrap();
    assert_eq!(
        results
            .iter()
            .map(|r| r.provider_id.as_str())
            .collect::<Vec<_>>(),
        ["p0", "p1", "p2", "p3"]
    );
    assert!(results[..3].iter().all(|r| r.status == "success"));
    assert_eq!(results[3].status, "failed");
    assert!(results[0].aggregation_key.is_none() && results[2].aggregation_key.is_none());
    assert!(results[1].aggregation_key.is_some());
    assert_eq!(results[1].currency.as_deref(), Some("USD"));
    assert!(!serde_json::to_string(&results)
        .unwrap()
        .contains("fixture-"));
    let single = query_provider(&db, &AppType::Claude, "p1").await.unwrap();
    assert_eq!(single.data[0].remaining, Some(12.5));
    assert_eq!(single.status, "success");
    assert!(query_provider(&db, &AppType::Claude, "p0")
        .await
        .unwrap()
        .aggregation_key
        .is_none());
    template.balance_scope = BalanceScope::Account;
    db.save_balance_query_template(&template).unwrap();
    assert!(query_group(&db, &group.id)
        .await
        .unwrap()
        .iter()
        .all(|r| r.aggregation_key.is_none()));
    task.abort();
    let _ = task.await;
}

#[test]
fn legacy_template_defaults_to_unknown_balance_scope() {
    let mut value = serde_json::to_value(template()).unwrap();
    value.as_object_mut().unwrap().remove("balanceScope");
    let legacy: BalanceQueryTemplate = serde_json::from_value(value).unwrap();
    assert_eq!(legacy.balance_scope, BalanceScope::Unknown);
}

#[tokio::test]
async fn provider_template_works_ungrouped_and_overrides_folder_for_single_and_batch_queries() {
    let router = Router::new()
        .route("/balance", get(|| async { Json(json!({"balance": 7.0})) }))
        .route(
            "/folder-balance",
            get(|| async { Json(json!({"balance": 19.0})) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let db = Database::memory().unwrap();
    let individual = template();
    let mut inherited = template();
    inherited.id = "folder-template".into();
    inherited.path = "/folder-balance".into();
    db.save_balance_query_template(&individual).unwrap();
    db.save_balance_query_template(&inherited).unwrap();
    let member = provider("standalone", &base, "fixture-key");
    db.save_provider("claude", &member).unwrap();
    db.set_provider_balance_template("claude", &member.id, Some(&individual.id))
        .unwrap();
    let result = query_provider(&db, &AppType::Claude, &member.id)
        .await
        .unwrap();
    assert_eq!(result.data[0].remaining, Some(7.0));
    assert!(db
        .provider_group_id("claude", &member.id)
        .unwrap()
        .is_none());
    let mut group = folder();
    group.balance_template_id = Some(inherited.id.clone());
    db.create_provider_group(&group).unwrap();
    db.assign_provider_group(
        "claude",
        &member.id,
        Some(&group.id),
        None,
        None,
        Some(true),
    )
    .unwrap();
    let result = query_provider(&db, &AppType::Claude, &member.id)
        .await
        .unwrap();
    assert_eq!(result.data[0].remaining, Some(7.0));
    assert_eq!(
        query_group(&db, &group.id).await.unwrap()[0].data[0].remaining,
        Some(7.0)
    );
    db.set_provider_balance_template("claude", &member.id, None)
        .unwrap();
    assert_eq!(
        query_group(&db, &group.id).await.unwrap()[0].data[0].remaining,
        Some(19.0)
    );
    db.set_provider_balance_template("claude", &member.id, Some(&individual.id))
        .unwrap();
    db.delete_balance_query_template(&individual.id).unwrap();
    let missing = query_group(&db, &group.id).await.unwrap();
    assert_eq!(missing[0].status, "failed");
    assert!(missing[0]
        .error
        .as_ref()
        .unwrap()
        .contains("balance_template_missing"));
    server.abort();
    let _ = server.await;
}

#[test]
fn provider_template_binding_is_scoped_and_preserves_provider_configuration() {
    let db = Database::memory().unwrap();
    let template = template();
    db.save_balance_query_template(&template).unwrap();
    let mut member = provider("same-id", "https://relay.example/v1", "fixture-key");
    member.meta = Some(ProviderMeta {
        provider_group_manual: Some(true),
        ..Default::default()
    });
    db.save_provider("claude", &member).unwrap();
    db.save_provider("codex", &member).unwrap();
    assert!(db
        .set_provider_balance_template("claude", &member.id, Some("missing"))
        .is_err());
    assert!(db
        .set_provider_balance_template("claude", "missing", Some(&template.id))
        .is_err());
    db.set_provider_balance_template("claude", &member.id, Some(&template.id))
        .unwrap();
    let saved = db
        .get_provider_by_id(&member.id, "claude")
        .unwrap()
        .unwrap();
    assert_eq!(saved.settings_config, member.settings_config);
    let meta = saved.meta.unwrap();
    assert_eq!(meta.provider_group_manual, Some(true));
    assert_eq!(
        meta.balance_template_id.as_deref(),
        Some(template.id.as_str())
    );
    assert!(db
        .get_provider_by_id(&member.id, "codex")
        .unwrap()
        .unwrap()
        .meta
        .unwrap()
        .balance_template_id
        .is_none());
    db.set_provider_balance_template("claude", &member.id, None)
        .unwrap();
    let saved = db
        .get_provider_by_id(&member.id, "claude")
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(saved).unwrap(),
        serde_json::to_value(member).unwrap()
    );
}
