use super::{
    input_error,
    target::{redact, KeyHeader, PinnedTarget},
    TargetInput, ValidationProtocol,
};
use crate::{database::Database, error::AppError, services::model_fetch::FetchedModel};
use serde_json::Value;
use std::time::Duration;
use url::Url;

pub async fn fetch_models(
    db: &Database,
    input: TargetInput,
) -> Result<Vec<FetchedModel>, AppError> {
    let target = PinnedTarget::resolve_for_models(db, &input)?;
    let models = tokio::time::timeout(Duration::from_secs(30), discover(&target))
        .await
        .map_err(|_| input_error("获取模型列表超时；可手动填写模型 ID"))??;
    target.verify_unchanged(db)?;
    Ok(models)
}

fn endpoints(target: &PinnedTarget) -> Result<Vec<String>, AppError> {
    let is_full = target
        .provider
        .meta
        .as_ref()
        .and_then(|meta| meta.is_full_url)
        .unwrap_or(false);
    if target.summary.protocol != ValidationProtocol::Gemini {
        return crate::services::model_fetch::build_models_url_candidates(
            &target.summary.endpoint,
            is_full,
            None,
        )
        .map_err(|_| input_error("无法根据 Base URL 推导模型列表地址"));
    }
    let mut url = Url::parse(&target.summary.endpoint).map_err(|_| input_error("Base URL 无效"))?;
    let path = url.path().trim_end_matches('/');
    let root = if is_full {
        path.split_once("/models/")
            .map(|(prefix, _)| prefix)
            .ok_or_else(|| input_error("无法根据 Gemini 完整地址推导模型列表地址"))?
    } else {
        path
    };
    let path = if ["/v1", "/v1beta", "/v1alpha"]
        .iter()
        .any(|version| root.ends_with(version))
    {
        format!("{root}/models")
    } else {
        format!("{root}/v1beta/models")
    };
    url.set_path(&path);
    Ok(vec![url.to_string()])
}

async fn discover(target: &PinnedTarget) -> Result<Vec<FetchedModel>, AppError> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(15))
        .user_agent("ccs-model-discovery/1")
        .build()
        .map_err(|_| input_error("无法创建模型列表请求"))?;
    let user_agent = crate::provider::parse_custom_user_agent(
        target
            .provider
            .meta
            .as_ref()
            .and_then(|meta| meta.custom_user_agent.as_deref()),
    )
    .map_err(|_| input_error("供应商 User-Agent 配置无效"))?;
    for endpoint in endpoints(target)? {
        let mut cursor: Option<(&str, String)> = None;
        let mut models = Vec::new();
        for page in 0..10 {
            let mut request = client.get(&endpoint);
            request = match target.header {
                KeyHeader::Bearer => request.bearer_auth(&target.key),
                KeyHeader::Anthropic => request.header("x-api-key", &target.key),
                KeyHeader::Google => request.header("x-goog-api-key", &target.key),
            };
            if target.summary.protocol == ValidationProtocol::Anthropic {
                request = request.header("anthropic-version", "2023-06-01");
            }
            if let Some(user_agent) = &user_agent {
                request = request.header(reqwest::header::USER_AGENT, user_agent.clone());
            }
            if let Some((field, value)) = &cursor {
                request = request.query(&[(*field, value)]);
            }
            let mut response = request
                .send()
                .await
                .map_err(|_| input_error("模型列表请求失败或超时；未切换凭据"))?;
            let status = response.status();
            if page == 0 && matches!(status.as_u16(), 404 | 405) {
                break;
            }
            if !status.is_success() {
                return Err(input_error(&format!(
                    "模型列表请求失败（HTTP {}）；可手动填写模型 ID",
                    status.as_u16()
                )));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| input_error("模型列表响应读取失败"))?
            {
                if body.len() + chunk.len() > 2_097_152 {
                    return Err(input_error("模型列表响应过大；可手动填写模型 ID"));
                }
                body.extend_from_slice(&chunk);
            }
            let payload: Value = serde_json::from_slice(&body)
                .map_err(|_| input_error("模型列表响应不是有效 JSON"))?;
            let entries = payload
                .get("data")
                .or_else(|| payload.get("models"))
                .and_then(Value::as_array)
                .ok_or_else(|| input_error("服务商没有返回兼容的模型列表；可手动填写模型 ID"))?;
            for entry in entries {
                let raw = entry
                    .get("id")
                    .or_else(|| entry.get("name"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| input_error("模型列表包含无效条目"))?;
                let model = if target.summary.protocol == ValidationProtocol::Gemini {
                    raw.strip_prefix("models/").unwrap_or(raw)
                } else {
                    raw
                };
                if model.is_empty()
                    || model.len() > 256
                    || model.trim() != model
                    || model.chars().any(char::is_control)
                    || redact(model, &[&target.key], 256) != model
                {
                    return Err(input_error("模型列表包含无效或敏感标识"));
                }
                models.push(FetchedModel {
                    id: model.to_string(),
                    owned_by: entry
                        .get("owned_by")
                        .and_then(Value::as_str)
                        .map(|owner| redact(owner, &[&target.key], 120)),
                    context_window: crate::services::model_fetch::extract_context_window(entry),
                });
            }
            if models.len() > 5000 {
                return Err(input_error("模型列表超过安全上限；可手动填写模型 ID"));
            }
            let next = if let Some(token) = payload
                .get("nextPageToken")
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
            {
                Some(("pageToken", token))
            } else if payload.get("has_more").and_then(Value::as_bool) == Some(true) {
                Some((
                    "after_id",
                    payload
                        .get("last_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| input_error("模型列表分页信息无效"))?,
                ))
            } else {
                None
            };
            let Some((field, value)) = next else {
                models.sort_by(|left, right| left.id.cmp(&right.id));
                models.dedup_by(|left, right| left.id == right.id);
                return Ok(models);
            };
            if value.is_empty()
                || value.len() > 2048
                || value.chars().any(char::is_control)
                || cursor
                    .as_ref()
                    .is_some_and(|(_, previous)| previous == value)
            {
                return Err(input_error("模型列表分页信息无效"));
            }
            cursor = Some((field, value.to_string()));
        }
        if cursor.is_some() {
            return Err(input_error("模型列表分页超过安全上限；可手动填写模型 ID"));
        }
    }
    Err(input_error(
        "服务商未开放模型列表接口（HTTP 404/405）；请手动填写模型 ID",
    ))
}
