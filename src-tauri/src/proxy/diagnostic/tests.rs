//! Exercise the real handlers and converters against loopback-only suppliers.
//! Never load a production database, client config, or credential.

use super::*;
use crate::{
    provider::{LocalProxyRequestOverrides, ProviderMeta},
    proxy::types::GlobalProxyConfig,
    services::model_validation::{
        ModelValidationService, PrepareRequest, Probe, RunStatus, TargetInput, ValidationMode,
        ValidationProtocol,
    },
};
use axum::{body::to_bytes, extract::Request, http::HeaderMap, routing::any, Router};
use serde_json::json;
use serial_test::serial;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsString,
    path::{Path, PathBuf},
};

const KEY_A: &str = "test-validation-key-A-not-real";
const KEY_B: &str = "test-validation-key-B-not-real";

struct TestHome {
    root: tempfile::TempDir,
    previous: Option<OsString>,
}

impl TestHome {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("CC_SWITCH_TEST_HOME");
        std::env::set_var("CC_SWITCH_TEST_HOME", root.path());
        // No AppHandle/store is initialized in these tests, so the override
        // cannot redirect OAuthManager::new away from this isolated home.
        assert!(crate::app_store::get_app_config_dir_override().is_none());
        crate::settings::reload_settings().unwrap();
        for (path, content) in [
            (
                ".codex/config.toml",
                "model = 'user-owned'\nmodel_catalog_json = 'catalog.json'\n",
            ),
            (
                ".codex/auth.json",
                r#"{"OPENAI_API_KEY":"synthetic-client-key"}"#,
            ),
            (
                ".codex/catalog.json",
                r#"{"models":[{"id":"user-owned","input_modalities":["text","image"],"custom":true}]}"#,
            ),
            (
                ".claude/settings.json",
                r#"{"env":{"ANTHROPIC_API_KEY":"synthetic-client-key"}}"#,
            ),
            (
                ".gemini/settings.json",
                r#"{"model":{"name":"user-owned"}}"#,
            ),
            (".grok/config.toml", "[models]\ndefault = 'user-owned'\n"),
        ] {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        Self { root, previous }
    }
    fn path(&self) -> &Path {
        self.root.path()
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        // Reload only while still inside the synthetic home. In particular,
        // never restore the cache by reading the real user's settings file.
        let _ = crate::settings::reload_settings();
        match &self.previous {
            Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
            None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
        }
    }
}

#[derive(Clone)]
struct MockResponse {
    status: u16,
    data: Vec<u8>,
    content_type: &'static str,
    location: Option<String>,
    delay: Duration,
}

impl MockResponse {
    fn json(value: Value) -> Self {
        Self {
            status: 200,
            data: serde_json::to_vec(&value).unwrap(),
            content_type: "application/json",
            location: None,
            delay: Duration::ZERO,
        }
    }
    fn error(status: u16, message: &str) -> Self {
        Self {
            status,
            ..Self::json(json!({"error":{"message":message,"type":"invalid_request_error"}}))
        }
    }
}

#[derive(Default)]
struct MockState {
    replies: Mutex<VecDeque<MockResponse>>,
    requests: Mutex<Vec<(String, HeaderMap, Value)>>,
}

struct MockServer {
    base: String,
    state: Arc<MockState>,
    task: tokio::task::JoinHandle<()>,
}

impl MockServer {
    async fn new(replies: Vec<MockResponse>) -> Self {
        async fn handle(State(state): State<Arc<MockState>>, request: Request) -> Response {
            let (parts, body) = request.into_parts();
            let body = to_bytes(body, 262_144).await.unwrap();
            state.requests.lock().unwrap().push((
                parts.uri.to_string(),
                parts.headers,
                serde_json::from_slice(&body).unwrap(),
            ));
            let reply = state
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| MockResponse::error(500, "unexpected synthetic request"));
            tokio::time::sleep(reply.delay).await;
            let mut response = Response::builder()
                .status(reply.status)
                .header("content-type", reply.content_type);
            if let Some(location) = reply.location {
                response = response.header("location", location);
            }
            response.body(Body::from(reply.data)).unwrap()
        }
        let state = Arc::new(MockState {
            replies: Mutex::new(replies.into()),
            ..Default::default()
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new()
            .fallback(any(handle))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { base, state, task }
    }
    fn count(&self) -> usize {
        self.state.requests.lock().unwrap().len()
    }
    async fn wait_for_request(&self) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.count() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn provider(app: AppType, protocol: &str, base: &str, key: &str) -> Provider {
    let config = match app {
        AppType::Claude | AppType::ClaudeDesktop => {
            if protocol.starts_with("openai") {
                json!({"env":{"ANTHROPIC_BASE_URL":base,"ANTHROPIC_AUTH_TOKEN":key}})
            } else {
                json!({"env":{"ANTHROPIC_BASE_URL":base,"ANTHROPIC_API_KEY":key}})
            }
        }
        AppType::Gemini => json!({"env":{"GOOGLE_GEMINI_BASE_URL":base,"GEMINI_API_KEY":key}}),
        AppType::GrokBuild => {
            json!({"config":format!("[models]\ndefault='proxy'\n[model.proxy]\nname='upstream-model'\nmodel='upstream-model'\nbase_url='{base}/v1'\napi_key='{key}'\napi_backend='responses'\ncontext_window=131072")})
        }
        _ => {
            json!({"auth":{"OPENAI_API_KEY":key},"config":format!("model_provider = 'mock'\nmodel = 'upstream-model'\n[model_providers.mock]\nbase_url = '{base}/v1'\nwire_api = 'responses'")})
        }
    };
    let mut provider =
        Provider::with_id("pinned-A".into(), "Synthetic supplier".into(), config, None);
    provider.meta = Some(ProviderMeta {
        api_format: Some(protocol.into()),
        api_key_field: Some("ANTHROPIC_API_KEY".into()),
        ..Default::default()
    });
    if app == AppType::ClaudeDesktop {
        let meta = provider.meta.as_mut().unwrap();
        meta.claude_desktop_mode = Some(crate::provider::ClaudeDesktopMode::Proxy);
        meta.claude_desktop_model_routes.insert(
            "claude-sonnet-4-6".into(),
            crate::provider::ClaudeDesktopModelRoute {
                model: "upstream-model".into(),
                label_override: None,
                supports_1m: Some(false),
            },
        );
    }
    provider
}

fn native_request(app: AppType, stream: bool, tokens: u64) -> (&'static str, Value) {
    match app {
        AppType::ClaudeDesktop => (
            "/v1/messages",
            json!({"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"Reply exactly CCS_OK."}],"max_tokens":tokens,"stream":stream}),
        ),
        AppType::Claude => (
            "/v1/messages",
            json!({"model":"requested-model","messages":[{"role":"user","content":"Reply exactly CCS_OK."}],"max_tokens":tokens,"stream":stream}),
        ),
        AppType::Gemini => (
            if stream {
                "/v1beta/models/requested-model:streamGenerateContent?alt=sse"
            } else {
                "/v1beta/models/requested-model:generateContent"
            },
            json!({"contents":[{"role":"user","parts":[{"text":"Reply exactly CCS_OK."}]}],"generationConfig":{"maxOutputTokens":tokens},"stream":stream}),
        ),
        _ => (
            "/responses",
            json!({"model":"requested-model","input":[{"role":"user","content":"Reply exactly CCS_OK."}],"max_output_tokens":tokens,"store":false,"stream":stream}),
        ),
    }
}

fn chat_response() -> Value {
    json!({"id":"chat-synthetic","object":"chat.completion","created":1,"model":"upstream-model","choices":[{"index":0,"message":{"role":"assistant","content":"CCS_OK"},"finish_reason":"stop"}],"usage":{"prompt_tokens":8,"completion_tokens":4,"total_tokens":12}})
}

fn responses_response() -> Value {
    json!({"id":"resp-synthetic","object":"response","created_at":1,"model":"upstream-model","status":"completed","output":[{"id":"msg-synthetic","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"CCS_OK","annotations":[]}]}],"usage":{"input_tokens":8,"output_tokens":4,"total_tokens":12}})
}

fn anthropic_response() -> Value {
    json!({"id":"msg-synthetic","type":"message","model":"upstream-model","role":"assistant","content":[{"type":"text","text":"CCS_OK"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":8,"output_tokens":4}})
}

fn file_snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            assert!(
                !kind.is_symlink(),
                "test files must stay inside the fixture"
            );
            if kind.is_dir() {
                visit(root, &entry.path(), files);
            } else {
                files.insert(
                    entry.path().strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(entry.path()).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

fn db_snapshot(db: &Database) -> (u64, BTreeMap<String, [u8; 32]>) {
    let conn = db.conn.lock().unwrap();
    let names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut tables = BTreeMap::new();
    for name in names {
        let sql = format!("SELECT * FROM \"{}\"", name.replace('"', "\"\""));
        let mut query = conn.prepare(&sql).unwrap();
        let columns = query.column_count();
        let mut rows: Vec<String> = query
            .query_map([], |row| {
                (0..columns)
                    .map(|i| row.get::<_, rusqlite::types::Value>(i))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|row| format!("{row:?}"))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        rows.sort();
        tables.insert(
            name,
            Sha256::digest(serde_json::to_vec(&rows).unwrap()).into(),
        );
    }
    let changes = conn
        .query_row("SELECT total_changes()", [], |row| row.get(0))
        .unwrap();
    (changes, tables)
}

async fn call(
    state: &AppState,
    app: AppType,
    provider: Provider,
    tokens: u64,
) -> (HeaderMap, Value) {
    let (path, body) = native_request(app.clone(), false, tokens);
    tokio::time::timeout(Duration::from_secs(5), async {
        let response = forward_validation(state, app, provider, "requested-model", path, body)
            .await
            .unwrap();
        let status = response.status();
        let (parts, body) = response.into_parts();
        let bytes = to_bytes(body, 1_048_576).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            status.is_success(),
            "synthetic diagnostic failed with {status}: {value}"
        );
        (parts.headers, value)
    })
    .await
    .expect("loopback diagnostic must be bounded")
}

#[tokio::test]
#[serial]
async fn model_validation_chain_uses_real_handlers_and_all_four_upstream_protocols() {
    let home = TestHome::new();
    let state = AppState::new(Arc::new(Database::memory().unwrap()));
    for (app, protocol, reply, path) in [
        (
            AppType::Claude,
            "openai_chat",
            chat_response(),
            "/v1/chat/completions",
        ),
        (
            AppType::Claude,
            "openai_responses",
            responses_response(),
            "/v1/responses",
        ),
        (
            AppType::ClaudeDesktop,
            "openai_chat",
            chat_response(),
            "/v1/chat/completions",
        ),
        (
            AppType::Codex,
            "anthropic",
            anthropic_response(),
            "/v1/messages",
        ),
        (
            AppType::Codex,
            "openai_chat",
            chat_response(),
            "/v1/chat/completions",
        ),
        (
            AppType::Codex,
            "openai_responses",
            responses_response(),
            "/v1/responses",
        ),
        (
            AppType::GrokBuild,
            "openai_chat",
            chat_response(),
            "/v1/chat/completions",
        ),
        (
            AppType::Gemini,
            "gemini",
            json!({"modelVersion":"requested-model","candidates":[{"content":{"role":"model","parts":[{"text":"CCS_OK"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":8,"candidatesTokenCount":4}}),
            "/v1beta/models/requested-model:generateContent",
        ),
    ] {
        let upstream = MockServer::new(vec![MockResponse::json(reply)]).await;
        let mut provider = provider(app.clone(), protocol, &upstream.base, KEY_A);
        provider
            .meta
            .as_mut()
            .unwrap()
            .local_proxy_request_overrides = Some(LocalProxyRequestOverrides {
            body: None,
            headers: [
                (" Authorization ".into(), format!("Bearer {KEY_B}")),
                (" X-API-Key ".into(), KEY_B.into()),
                ("X-Goog-Api-Key".into(), KEY_B.into()),
                ("Proxy-Authorization".into(), format!("Bearer {KEY_B}")),
            ]
            .into(),
        });
        let mut current = provider.clone();
        current.id = "production-B".into();
        state.db.save_provider(app.as_str(), &current).unwrap();
        state
            .db
            .set_current_provider(app.as_str(), &current.id)
            .unwrap();
        let before_db = db_snapshot(&state.db);
        let before_files = file_snapshot(home.path());
        let before_status =
            serde_json::to_value(state.proxy_service.get_status().await.unwrap()).unwrap();
        let (headers, response) = call(&state, app.clone(), provider, 256).await;
        let records = upstream.state.requests.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, path, "{app:?}/{protocol}");
        let (_, sent_headers, sent) = &records[0];
        assert!(!sent_headers
            .values()
            .any(|value| value.to_str().is_ok_and(|text| text.contains(KEY_B))));
        assert!(!sent_headers.contains_key("proxy-authorization"));
        if protocol == "anthropic" {
            assert_eq!(sent_headers["x-api-key"], KEY_A);
            assert!(sent.get("messages").is_some() && sent.get("input").is_none());
        } else if app == AppType::Gemini {
            assert_eq!(sent_headers["x-goog-api-key"], KEY_A);
            assert!(sent.get("contents").is_some() && sent.get("stream").is_none());
        } else {
            assert_eq!(sent_headers["authorization"], format!("Bearer {KEY_A}"));
            assert_eq!(sent.get("messages").is_some(), protocol == "openai_chat");
        }
        let actual_model = sent
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("requested-model");
        assert_eq!(headers["x-ccs-validation-upstream-model"], actual_model);
        if matches!(app, AppType::Claude | AppType::ClaudeDesktop) {
            assert_eq!(response["type"], "message");
            assert_eq!(response["content"][0]["text"], "CCS_OK");
        } else if app == AppType::Gemini {
            assert_eq!(
                response["candidates"][0]["content"]["parts"][0]["text"],
                "CCS_OK"
            );
        } else {
            assert_eq!(response["object"], "response");
            assert!(response["output"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["content"]
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(|part| part["text"] == "CCS_OK"))));
        }
        assert_eq!(db_snapshot(&state.db), before_db, "{app:?}/{protocol}");
        assert_eq!(
            file_snapshot(home.path()),
            before_files,
            "{app:?}/{protocol}"
        );
        assert_eq!(
            serde_json::to_value(state.proxy_service.get_status().await.unwrap()).unwrap(),
            before_status
        );
    }
}

#[tokio::test]
#[serial]
async fn model_validation_chain_limits_survive_transforms_and_provider_overrides() {
    let _home = TestHome::new();
    let state = AppState::new(Arc::new(Database::memory().unwrap()));
    let upstream = MockServer::new(vec![MockResponse::json(anthropic_response())]).await;
    let mut provider = provider(AppType::Codex, "anthropic", &upstream.base, KEY_A);
    provider
        .meta
        .as_mut()
        .unwrap()
        .local_proxy_request_overrides = Some(LocalProxyRequestOverrides {
        body: Some(json!({"max_tokens":100_000,"n":9})),
        headers: [
            ("authorization".into(), format!("Bearer {KEY_B}")),
            ("x-api-key".into(), KEY_B.into()),
        ]
        .into(),
    });
    call(&state, AppType::Codex, provider, 16).await;
    let records = upstream.state.requests.lock().unwrap();
    assert_eq!(records[0].2["max_tokens"], 16);
    assert_eq!(records[0].2["n"], 1);
    assert_eq!(records[0].1["x-api-key"], KEY_A);
    assert!(!records[0].1.contains_key("authorization"));
}

#[tokio::test]
#[serial]
async fn model_validation_chain_rejects_non_synthetic_overrides_before_sending() {
    let home = TestHome::new();
    let state = AppState::new(Arc::new(Database::memory().unwrap()));
    let upstream = MockServer::new(vec![]).await;
    let before_files = file_snapshot(home.path());
    let before_db = db_snapshot(&state.db);
    for overrides in [
        json!({"messages":[{"role":"user","content":"private-fixture"}]}),
        json!({"input":"private-fixture"}),
        json!({"instructions":"private-fixture"}),
        json!({"system":"private-fixture"}),
        json!({"tools":[{"type":"web_search"}]}),
        json!({"previous_response_id":"private-fixture"}),
        json!({"cachedContent":"private-fixture"}),
        json!({"generationConfig":{"private":"private-fixture"}}),
        json!({"store":true}),
        json!({"background":true}),
    ] {
        let mut provider = provider(AppType::Codex, "openai_responses", &upstream.base, KEY_A);
        provider
            .meta
            .as_mut()
            .unwrap()
            .local_proxy_request_overrides = Some(LocalProxyRequestOverrides {
            body: Some(overrides),
            headers: Default::default(),
        });
        let (path, body) = native_request(AppType::Codex, false, 256);
        let result = forward_validation(
            &state,
            AppType::Codex,
            provider,
            "requested-model",
            path,
            body,
        )
        .await;
        assert!(result.is_err());
        assert!(!result
            .err()
            .unwrap()
            .to_string()
            .contains("private-fixture"));
    }
    assert_eq!(upstream.count(), 0);
    assert_eq!(file_snapshot(home.path()), before_files);
    assert_eq!(db_snapshot(&state.db), before_db);
}

#[tokio::test]
#[serial]
async fn model_validation_chain_official_label_does_not_reject_explicit_api_keys() {
    let _home = TestHome::new();
    let state = AppState::new(Arc::new(Database::memory().unwrap()));
    let upstream = MockServer::new(vec![MockResponse::json(responses_response())]).await;
    let mut provider = provider(AppType::Codex, "openai_responses", &upstream.base, KEY_A);
    provider.category = Some("official".into());
    call(&state, AppType::Codex, provider.clone(), 256).await;
    provider.meta.as_mut().unwrap().provider_type = Some("codex_oauth".into());
    let (path, body) = native_request(AppType::Codex, false, 256);
    assert!(forward_validation(
        &state,
        AppType::Codex,
        provider,
        "requested-model",
        path,
        body
    )
    .await
    .is_err());
    assert_eq!(upstream.count(), 1);
}

#[test]
fn model_validation_chain_rejects_unbounded_or_invalid_limits() {
    for body in [
        json!({}),
        json!({"max_tokens":0}),
        json!({"max_tokens":2049}),
    ] {
        assert!(DiagnosticContext::new(&body).is_err());
    }
    let context = DiagnosticContext::new(&json!({"max_output_tokens":16})).unwrap();
    for mut body in [
        json!({}),
        json!({"max_tokens":null}),
        json!({"max_tokens":-1}),
    ] {
        assert!(context.enforce_output_limits(&mut body).is_err());
    }
    let mut gemini = json!({"generationConfig":{"maxOutputTokens":1_000_000,"candidateCount":7}});
    context.enforce_output_limits(&mut gemini).unwrap();
    assert_eq!(gemini["generationConfig"]["maxOutputTokens"], 16);
    assert_eq!(gemini["generationConfig"]["candidateCount"], 1);
    let controlled = DiagnosticContext::new(&json!({"max_tokens":128,"temperature":0})).unwrap();
    for mut changed in [
        json!({"max_output_tokens":128}),
        json!({"max_output_tokens":128,"temperature":1}),
        json!({"generationConfig":{"maxOutputTokens":128,"temperature":0.2}}),
    ] {
        assert!(controlled.enforce_output_limits(&mut changed).is_err());
    }
    let mut preserved = json!({"generationConfig":{"maxOutputTokens":128,"temperature":0}});
    controlled.enforce_output_limits(&mut preserved).unwrap();
}

#[tokio::test]
#[serial]
async fn model_validation_chain_stops_comparison_temperature_overrides_before_sending() {
    let _home = TestHome::new();
    let state = AppState::new(Arc::new(Database::memory().unwrap()));
    let upstream = MockServer::new(vec![]).await;
    let mut target = provider(AppType::Claude, "openai_chat", &upstream.base, KEY_A);
    target.meta.as_mut().unwrap().local_proxy_request_overrides =
        Some(LocalProxyRequestOverrides {
            body: Some(json!({"temperature":0.9})),
            headers: Default::default(),
        });
    let (path, mut body) = native_request(AppType::Claude, false, 128);
    body["temperature"] = json!(0);
    let response = forward_validation(
        &state,
        AppType::Claude,
        target,
        "requested-model",
        path,
        body,
    )
    .await
    .unwrap();
    assert!(!response.status().is_success());
    assert_eq!(upstream.count(), 0);
}

#[tokio::test]
async fn model_validation_chain_response_cap_stops_chunks_before_generic_buffering() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let context = DiagnosticContext::new(&json!({"max_output_tokens":16})).unwrap();
    let polled = Arc::new(AtomicUsize::new(0));
    let count = polled.clone();
    let stream = futures::stream::iter(0..4).map(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(bytes::Bytes::from(vec![b'x'; 524_288]))
    });
    let raw = ProxyResponse::streamed(http::StatusCode::OK, HeaderMap::new(), stream);
    let bounded = context.limit_response(raw).unwrap();
    let result = bounded
        .bytes_with_limit(super::super::hyper_client::MAX_RESPONSE_BODY_BYTES)
        .await;
    assert!(result.is_err());
    assert_eq!(
        polled.load(Ordering::SeqCst),
        3,
        "the fourth chunk is never consumed"
    );
}

#[test]
fn model_validation_chain_response_cap_rejects_large_or_encoded_headers_without_reading() {
    let context = DiagnosticContext::new(&json!({"max_output_tokens":16})).unwrap();
    for (name, value) in [
        (http::header::CONTENT_LENGTH, "1048577"),
        (http::header::CONTENT_ENCODING, "gzip"),
        (http::header::CONTENT_ENCODING, "br"),
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(name, HeaderValue::from_static(value));
        let raw = ProxyResponse::streamed(
            http::StatusCode::OK,
            headers,
            futures::stream::poll_fn(
                |_| -> std::task::Poll<Option<Result<bytes::Bytes, std::io::Error>>> {
                    panic!("invalid declared response must be rejected before any body poll")
                },
            ),
        );
        assert!(context.limit_response(raw).is_err());
    }
}

#[tokio::test]
#[serial]
async fn model_validation_chain_caps_upstream_metadata_before_conversion_can_discard_it() {
    let home = TestHome::new();
    let state = AppState::new(Arc::new(Database::memory().unwrap()));
    let mut reply = chat_response();
    reply["discarded_metadata"] = Value::String("x".repeat(1_048_576));
    let upstream = MockServer::new(vec![MockResponse::json(reply)]).await;
    let target = provider(AppType::Claude, "openai_chat", &upstream.base, KEY_A);
    let before_db = db_snapshot(&state.db);
    let before_files = file_snapshot(home.path());
    let (path, body) = native_request(AppType::Claude, false, 256);
    let response = forward_validation(
        &state,
        AppType::Claude,
        target,
        "requested-model",
        path,
        body,
    )
    .await
    .unwrap();
    assert!(
        !response.status().is_success(),
        "conversion must not conceal upstream overflow"
    );
    let _ = to_bytes(response.into_body(), 1_048_576).await.unwrap();
    let requests = upstream.state.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1["accept-encoding"], "identity");
    assert_eq!(db_snapshot(&state.db), before_db);
    assert_eq!(file_snapshot(home.path()), before_files);
}

#[tokio::test]
#[serial]
async fn model_validation_chain_never_follows_redirects_or_retries_rejections() {
    let home = TestHome::new();
    let state = AppState::new(Arc::new(Database::memory().unwrap()));
    let replacement = MockServer::new(vec![MockResponse::json(responses_response())]).await;
    let mut redirect = MockResponse::error(307, "synthetic redirect");
    redirect.location = Some(format!("{}/v1/responses", replacement.base));
    for rejected in [
        redirect,
        MockResponse::error(401, "synthetic auth failure"),
        MockResponse::error(503, "synthetic temporary failure"),
    ] {
        let primary =
            MockServer::new(vec![rejected, MockResponse::json(responses_response())]).await;
        let target = provider(AppType::Codex, "openai_responses", &primary.base, KEY_A);
        let before_db = db_snapshot(&state.db);
        let before_files = file_snapshot(home.path());
        let (path, body) = native_request(AppType::Codex, false, 256);
        let reply = tokio::time::timeout(
            Duration::from_secs(5),
            forward_validation(
                &state,
                AppType::Codex,
                target,
                "requested-model",
                path,
                body,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!reply.status().is_success());
        let _ = to_bytes(reply.into_body(), 1_048_576).await.unwrap();
        assert_eq!(primary.count(), 1);
        assert_eq!(replacement.count(), 0);
        assert_eq!(db_snapshot(&state.db), before_db);
        assert_eq!(file_snapshot(home.path()), before_files);
    }
}

#[tokio::test]
#[serial]
async fn model_validation_chain_cancellation_stops_later_probes_without_live_writes() {
    let home = TestHome::new();
    let mut slow = MockResponse::json(responses_response());
    slow.delay = Duration::from_secs(10);
    let upstream = MockServer::new(vec![slow, MockResponse::json(responses_response())]).await;
    let state = AppState::new(Arc::new(Database::memory().unwrap()));
    let target = provider(AppType::Codex, "openai_responses", &upstream.base, KEY_A);
    state.db.save_provider("codex", &target).unwrap();
    let before_provider =
        serde_json::to_value(state.db.get_all_providers("codex").unwrap()).unwrap();
    let before_files = file_snapshot(home.path());
    let plan = ModelValidationService::prepare(
        &state,
        PrepareRequest {
            target: TargetInput {
                app_id: "codex".into(),
                provider_id: target.id,
                model: "requested-model".into(),
                protocol: Some(ValidationProtocol::OpenaiResponses),
            },
            mode: ValidationMode::Ccs,
            probes: vec![Probe::Call, Probe::Structured],
            comparison_target: None,
            repeat_count: None,
        },
    )
    .unwrap();
    let initial = ModelValidationService::start(&state, &plan.id).unwrap();
    upstream.wait_for_request().await;
    assert!(ModelValidationService::cancel(&state, &initial.id).unwrap());
    let final_run = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let run = ModelValidationService::get(&state, &initial.id).unwrap();
            if run.status != RunStatus::Running {
                break run;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(final_run.status, RunStatus::Cancelled);
    assert_eq!(final_run.results[0].request_count, 1);
    assert_eq!(final_run.results[1].request_count, 0);
    assert_eq!(upstream.count(), 1);
    assert_eq!(file_snapshot(home.path()), before_files);
    assert_eq!(
        serde_json::to_value(state.db.get_all_providers("codex").unwrap()).unwrap(),
        before_provider
    );
    assert!(!serde_json::to_string(&final_run).unwrap().contains(KEY_A));
}

// The live proxy fixture explicitly tunnels through the loopback mock. It never
// inherits a developer's environment proxy or changes desktop-process state.
struct LocalProxySetting {
    previous: Option<String>,
}
impl LocalProxySetting {
    fn new(url: &str) -> Self {
        let previous = super::super::http_client::get_current_proxy_url();
        super::super::http_client::update_proxy(Some(url)).unwrap();
        Self { previous }
    }
}
impl Drop for LocalProxySetting {
    fn drop(&mut self) {
        super::super::http_client::update_proxy(self.previous.as_deref()).unwrap();
    }
}

async fn pool_status(state: &AppState, group: &str) -> Value {
    let mut status =
        crate::services::provider_groups::ProviderGroupService::group_status(&state.db, group)
            .unwrap();
    state.proxy_service.fill_key_pool_status(&mut status).await;
    // Clock passage is not a route-state mutation.
    let mut value = serde_json::to_value(status).unwrap();
    for member in value["members"].as_array_mut().unwrap() {
        member
            .as_object_mut()
            .unwrap()
            .remove("cooldownRemainingMs");
    }
    value
}

async fn live_status(state: &AppState) -> Value {
    let mut value = serde_json::to_value(state.proxy_service.get_status().await.unwrap()).unwrap();
    value.as_object_mut().unwrap().remove("uptime_seconds");
    value
}

#[tokio::test]
#[serial]
async fn model_validation_chain_leaves_running_proxy_breakers_key_rotation_and_clients_untouched() {
    use crate::provider_groups::{KeyPoolStrategy, ProviderGroup, ProviderGroupKind};
    let home = TestHome::new();
    let upstream = MockServer::new(vec![
        MockResponse::json(responses_response()), // production warmup A
        MockResponse::error(429, "synthetic diagnostic A failure"),
        MockResponse::error(429, "synthetic diagnostic A failure"),
        MockResponse::error(429, "synthetic diagnostic A failure"),
        MockResponse::json(responses_response()), // production must still use B next
        MockResponse::json(responses_response()), // A must not have been put into circuit-open
    ])
    .await;
    let _proxy_setting = LocalProxySetting::new(&upstream.base);
    let state = AppState::new(Arc::new(Database::memory().unwrap()));
    let group = ProviderGroup {
        id: "diagnostic-pool".into(),
        app_type: "codex".into(),
        name: "Synthetic pool".into(),
        icon: None,
        icon_color: None,
        kind: ProviderGroupKind::Manual,
        normalized_base_url: None,
        sort_index: 0,
        collapsed: false,
        key_pool_enabled: true,
        key_pool_strategy: KeyPoolStrategy::RoundRobin,
        key_pool_max_retries: 0,
        key_pool_cooldown_ms: 60_000,
        balance_template_id: None,
        created_at: 1,
        updated_at: 1,
    };
    state.db.create_provider_group(&group).unwrap();
    let mut a = provider(AppType::Codex, "openai_responses", &upstream.base, KEY_A);
    a.meta.as_mut().unwrap().provider_group_id = Some(group.id.clone());
    a.meta.as_mut().unwrap().key_pool_enabled = Some(true);
    a.meta.as_mut().unwrap().provider_group_sort_index = Some(0);
    let mut b = provider(AppType::Codex, "openai_responses", &upstream.base, KEY_B);
    b.id = "pool-B".into();
    b.meta.as_mut().unwrap().provider_group_id = Some(group.id.clone());
    b.meta.as_mut().unwrap().key_pool_enabled = Some(true);
    b.meta.as_mut().unwrap().provider_group_sort_index = Some(1);
    state.db.save_provider("codex", &a).unwrap();
    state.db.save_provider("codex", &b).unwrap();
    state.db.set_current_provider("codex", &a.id).unwrap();
    crate::settings::set_current_provider(&AppType::Codex, Some(&a.id)).unwrap();
    state
        .db
        .update_global_proxy_config(GlobalProxyConfig {
            proxy_enabled: false,
            listen_address: "127.0.0.1".into(),
            listen_port: 0,
            enable_logging: false,
        })
        .await
        .unwrap();
    let mut policy = state.db.get_proxy_config_for_app("codex").await.unwrap();
    policy.circuit_failure_threshold = 1;
    policy.circuit_min_requests = 1;
    policy.circuit_timeout_seconds = 600;
    state.db.update_proxy_config_for_app(policy).await.unwrap();
    let info = state.proxy_service.start().await.unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let (_, body) = native_request(AppType::Codex, false, 256);
    let url = format!("http://{}:{}/responses", info.address, info.port);
    let warmup = client.post(&url).json(&body).send().await.unwrap();
    assert!(warmup.status().is_success());
    let _: Value = warmup.json().await.unwrap();
    let before_db = db_snapshot(&state.db);
    let before_files = file_snapshot(home.path());
    let before_pool = pool_status(&state, &group.id).await;
    let before_status = live_status(&state).await;
    assert_eq!(upstream.count(), 1);
    for _ in 0..3 {
        let response = forward_validation(
            &state,
            AppType::Codex,
            a.clone(),
            "requested-model",
            "/responses",
            body.clone(),
        )
        .await
        .unwrap();
        assert!(!response.status().is_success());
        let _ = to_bytes(response.into_body(), 1_048_576).await.unwrap();
    }
    assert_eq!(upstream.count(), 4);
    assert_eq!(
        db_snapshot(&state.db),
        before_db,
        "no writes, even if later reverted, to the production connection"
    );
    assert_eq!(file_snapshot(home.path()), before_files);
    assert_eq!(pool_status(&state, &group.id).await, before_pool);
    assert_eq!(live_status(&state).await, before_status);
    assert_eq!(
        state.db.get_current_provider("codex").unwrap(),
        Some(a.id.clone())
    );
    assert_eq!(
        crate::settings::get_current_provider(&AppType::Codex),
        Some(a.id)
    );
    for _ in 0..2 {
        let reply = client.post(&url).json(&body).send().await.unwrap();
        assert!(reply.status().is_success());
        let _: Value = reply.json().await.unwrap();
    }
    let expected = [KEY_A, KEY_A, KEY_A, KEY_A, KEY_B, KEY_A];
    {
        let records = upstream.state.requests.lock().unwrap();
        assert_eq!(records.len(), expected.len());
        for ((_, headers, sent), key) in records.iter().zip(expected) {
            assert_eq!(headers["authorization"], format!("Bearer {key}"));
            assert_eq!(sent["model"], body["model"]);
        }
    }
    state.proxy_service.stop().await.unwrap();
}
