//! No retries, redirects, key selection, implicit environment proxy or live config.

use super::{
    protocol::{self, Observation, SseDecoder},
    target::PinnedTarget,
    ValidationMode,
};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client,
};
use serde_json::Value;
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};

pub(super) const REQUEST_TIMEOUT: u32 = 45;
const MAX_BODY_BYTES: usize = 1_048_576;
const MAX_REQUEST_BYTES: usize = 262_144;
type DataStream = Pin<Box<dyn Stream<Item = Result<Bytes, RequestFailure>> + Send>>;

#[derive(Clone, Default)]
pub(super) struct Cancellation {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Cancellation {
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
    pub async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RequestFailure {
    Cancelled,
    Timeout,
    Network,
    TooLarge,
    InvalidResponse,
    Budget,
    Chain,
}

impl RequestFailure {
    pub fn message(self) -> &'static str {
        match self {
            Self::Cancelled => "检测已取消；未启动后续请求",
            Self::Timeout => "请求或运行超过时限；未自动重试",
            Self::Network => "网络、TLS 或连接失败；未切换端点或凭据",
            Self::TooLarge => "请求或响应超过诊断安全上限",
            Self::InvalidResponse => "响应不是有效的预期协议数据",
            Self::Budget => "已达到预览中的请求预算，停止后续请求",
            Self::Chain => "ccs 转发链执行失败（原始错误已脱敏）",
        }
    }
}

pub(super) struct Reply {
    pub status: u16,
    pub observation: Observation,
    pub value: Value,
    pub sse: Option<SseDecoder>,
    pub elapsed_ms: u64,
    pub upstream_model: Option<String>,
    pub channel_hints: Vec<&'static str>,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status) && !self.observation.failed
    }
    pub fn semantic(&self, expected: &str) -> bool {
        self.ok() && self.observation.text.trim() == expected
    }
    pub fn signature_rejection(&self) -> bool {
        if !matches!(self.status, 400 | 422) {
            return false;
        }
        let message = self
            .value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        let code = self
            .value
            .pointer("/error/code")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        let text = format!("{code} {message}");
        text.contains("signature")
            && [
                "invalid",
                "verification failed",
                "verify",
                "mismatch",
                "tamper",
                "not valid",
            ]
            .iter()
            .any(|p| text.contains(p))
            && ![
                "unknown field",
                "unsupported",
                "unrecognized",
                "additional propert",
                "must be a string",
                "required",
            ]
            .iter()
            .any(|p| text.contains(p))
    }
    pub fn unsupported_parameter(&self) -> bool {
        if !matches!(self.status, 400 | 422) {
            return false;
        }
        let text = self
            .value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        [
            "not support",
            "unsupported",
            "not available for this model",
            "unknown parameter",
            "unrecognized request argument",
        ]
        .iter()
        .any(|p| text.contains(p))
    }
}

pub(super) struct Executor {
    client: Client,
    pub mode: ValidationMode,
    state: Option<Arc<crate::store::AppState>>,
    pub cancellation: Cancellation,
    deadline: Instant,
    pub requests: u32,
    max_requests: u32,
}

impl Executor {
    pub fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }
    pub fn new(
        mode: ValidationMode,
        state: Option<Arc<crate::store::AppState>>,
        cancellation: Cancellation,
        max_requests: u32,
        duration: u32,
    ) -> Result<Self, RequestFailure> {
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(REQUEST_TIMEOUT as u64))
            .pool_max_idle_per_host(0)
            .user_agent("ccs-model-validation/1")
            .build()
            .map_err(|_| RequestFailure::Network)?;
        Ok(Self {
            client,
            mode,
            state,
            cancellation,
            deadline: Instant::now() + Duration::from_secs(duration as u64),
            requests: 0,
            max_requests,
        })
    }

    pub async fn send(
        &mut self,
        target: &PinnedTarget,
        mut body: Value,
    ) -> Result<Reply, RequestFailure> {
        if self.cancellation.is_cancelled() {
            return Err(RequestFailure::Cancelled);
        }
        if self.requests >= self.max_requests {
            return Err(RequestFailure::Budget);
        }
        if Instant::now() >= self.deadline {
            return Err(RequestFailure::Timeout);
        }
        if serde_json::to_vec(&body)
            .map_err(|_| RequestFailure::TooLarge)?
            .len()
            > MAX_REQUEST_BYTES
        {
            return Err(RequestFailure::TooLarge);
        }
        let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        let protocol = target.wire_protocol(self.mode);
        let path = protocol::endpoint(protocol, &target.summary.model, stream);
        if protocol == super::ValidationProtocol::OpenaiChat && stream {
            body["stream_options"] = serde_json::json!({"include_usage":true});
        }
        self.requests += 1;
        let started = Instant::now();
        let operation = async {
            let (status, headers, data): (u16, HeaderMap, DataStream) = if self.mode
                == ValidationMode::Direct
            {
                if protocol == super::ValidationProtocol::Gemini {
                    body.as_object_mut()
                        .ok_or(RequestFailure::InvalidResponse)?
                        .remove("stream");
                }
                let url = if target
                    .provider
                    .meta
                    .as_ref()
                    .is_some_and(|meta| meta.is_full_url == Some(true))
                {
                    target.summary.endpoint.clone()
                } else {
                    direct_url(&target.summary.endpoint, &path, protocol)
                };
                let mut secret =
                    HeaderValue::from_str(&target.key).map_err(|_| RequestFailure::Network)?;
                secret.set_sensitive(true);
                let mut request = self.client.post(url).header(
                    "Accept",
                    if stream {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                );
                request = match target.header {
                    super::target::KeyHeader::Bearer => request.bearer_auth(&target.key),
                    super::target::KeyHeader::Anthropic => request.header("x-api-key", secret),
                    super::target::KeyHeader::Google => request.header("x-goog-api-key", secret),
                };
                if protocol == super::ValidationProtocol::Anthropic {
                    request = request.header("anthropic-version", "2023-06-01");
                }
                let response = request.json(&body).send().await.map_err(network_error)?;
                (
                    response.status().as_u16(),
                    response.headers().clone(),
                    Box::pin(response.bytes_stream().map(|r| r.map_err(network_error))),
                )
            } else {
                let state = self.state.as_ref().ok_or(RequestFailure::Chain)?;
                let response = crate::proxy::diagnostic::forward_validation(
                    state,
                    target.app.clone(),
                    target.provider.clone(),
                    &target.summary.model,
                    &path,
                    body,
                )
                .await
                .map_err(|_| RequestFailure::Chain)?;
                let (parts, body) = response.into_parts();
                (
                    parts.status.as_u16(),
                    parts.headers,
                    Box::pin(
                        body.into_data_stream()
                            .map(|r| r.map_err(|_| RequestFailure::Network)),
                    ),
                )
            };
            collect(status, headers, data, protocol, stream, started).await
        };
        let request_deadline = std::cmp::min(
            self.deadline,
            started + Duration::from_secs(REQUEST_TIMEOUT as u64),
        );
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => Err(RequestFailure::Cancelled),
            _ = tokio::time::sleep_until(request_deadline) => Err(RequestFailure::Timeout),
            result = operation => result,
        }
    }
}

fn network_error(error: reqwest::Error) -> RequestFailure {
    if error.is_timeout() {
        RequestFailure::Timeout
    } else {
        RequestFailure::Network
    }
}

pub(super) fn direct_url(base: &str, path: &str, protocol: super::ValidationProtocol) -> String {
    let mut base = base.trim_end_matches('/').to_string();
    let full_suffix = match protocol {
        super::ValidationProtocol::OpenaiChat => Some("/chat/completions"),
        super::ValidationProtocol::OpenaiResponses => Some("/responses"),
        super::ValidationProtocol::Anthropic => Some("/messages"),
        super::ValidationProtocol::Gemini => None,
    };
    if full_suffix.is_some_and(|suffix| base.ends_with(suffix)) {
        return base;
    }
    // Permit a configured full API endpoint as well as a conventional API root.
    for suffix in ["/chat/completions", "/responses", "/messages"] {
        if base.ends_with(suffix) {
            base.truncate(base.len() - suffix.len());
            break;
        }
    }
    let mut path = path.to_string();
    let origin_only = url::Url::parse(&base).is_ok_and(|url| matches!(url.path(), "" | "/"));
    if protocol == super::ValidationProtocol::OpenaiResponses
        && origin_only
        && !base.ends_with("/v1")
        && !base.ends_with("/v1beta")
    {
        path = format!("/v1{path}");
    }
    if protocol == super::ValidationProtocol::OpenaiChat
        && !origin_only
        && !base.ends_with("/v1")
        && !base.ends_with("/v1beta")
    {
        // OpenAI-compatible Base URLs with a custom path are already API roots.
        path = path.strip_prefix("/v1").unwrap_or(&path).to_string();
    }
    for version in ["/v1beta", "/v1"] {
        if (base.ends_with("/v1") || base.ends_with("/v1beta"))
            && path.starts_with(&format!("{version}/"))
        {
            path = path[version.len()..].to_string();
            break;
        }
    }
    format!("{base}{path}")
}

async fn collect(
    status: u16,
    headers: HeaderMap,
    mut data: DataStream,
    protocol: super::ValidationProtocol,
    requested_stream: bool,
    started: Instant,
) -> Result<Reply, RequestFailure> {
    let is_sse = headers
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().starts_with("text/event-stream"));
    let mut sse = (requested_stream && is_sse && (200..300).contains(&status))
        .then(|| SseDecoder::new(protocol));
    let mut bytes = Vec::new();
    while let Some(chunk) = data.next().await {
        let chunk = chunk?;
        if bytes.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            return Err(RequestFailure::TooLarge);
        }
        if let Some(decoder) = sse.as_mut() {
            decoder
                .feed(&chunk, elapsed(started))
                .map_err(|_| RequestFailure::InvalidResponse)?;
        }
        bytes.extend_from_slice(&chunk);
    }
    let value = if sse.is_some() {
        Value::Null
    } else {
        match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) if !(200..300).contains(&status) => Value::Null,
            Err(_) => return Err(RequestFailure::InvalidResponse),
        }
    };
    let observation = if let Some(decoder) = sse.as_ref() {
        decoder.observation.clone()
    } else {
        protocol::observe(protocol, &value)
    };
    // Only keyword presence survives. Arbitrary response headers/body text never enters evidence.
    let body_text = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
    let mut hints = Vec::new();
    if body_text.contains("amazon-bedrock") || headers.contains_key("x-amzn-requestid") {
        hints.push("检测到 Bedrock 相关文本/响应头线索；不是来源认证");
    }
    if body_text.contains("aiplatform.googleapis.com") {
        hints.push("检测到 Vertex 相关文本线索；不是来源认证");
    }
    let upstream_model = headers
        .get("x-ccs-validation-upstream-model")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    Ok(Reply {
        status,
        observation,
        value,
        sse,
        elapsed_ms: elapsed(started),
        upstream_model,
        channel_hints: hints,
    })
}

pub(super) fn elapsed(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}
