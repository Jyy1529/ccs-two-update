use super::*;
use super::{
    protocol::{self, SseDecoder},
    runtime::Runtime,
    target::PinnedTarget,
    transport::{Cancellation, Executor, RequestFailure},
};
use crate::{
    app_config::AppType,
    database::Database,
    provider::{Provider, ProviderMeta},
};
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::any,
    Router,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

const FAKE_KEY: &str = "test-static-key-A-not-a-real-credential";

struct MockResponse {
    status: u16,
    data: Vec<u8>,
    sse: bool,
    delay: Duration,
    location: Option<String>,
}

impl MockResponse {
    fn json(status: u16, value: Value) -> Self {
        Self {
            status,
            data: serde_json::to_vec(&value).unwrap(),
            sse: false,
            delay: Duration::ZERO,
            location: None,
        }
    }
    fn sse(data: &str) -> Self {
        Self {
            status: 200,
            data: data.as_bytes().to_vec(),
            sse: true,
            delay: Duration::ZERO,
            location: None,
        }
    }
}

#[derive(Default)]
struct MockState {
    pending: Mutex<VecDeque<MockResponse>>,
    requests: Mutex<Vec<(String, HeaderMap, Value)>>,
}

struct MockServer {
    base: String,
    state: Arc<MockState>,
    task: tokio::task::JoinHandle<()>,
}

impl MockServer {
    async fn new(responses: Vec<MockResponse>) -> Self {
        let state = Arc::new(MockState {
            pending: Mutex::new(responses.into()),
            ..Default::default()
        });
        async fn handler(State(state): State<Arc<MockState>>, request: Request) -> Response {
            let (parts, body) = request.into_parts();
            let data = to_bytes(body, 300_000).await.unwrap();
            let value = if data.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&data).unwrap()
            };
            state
                .requests
                .lock()
                .unwrap()
                .push((parts.uri.to_string(), parts.headers, value));
            let mock = state
                .pending
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    MockResponse::json(500, json!({"error":{"message":"unexpected mock request"}}))
                });
            tokio::time::sleep(mock.delay).await;
            let mut response = Response::builder()
                .status(StatusCode::from_u16(mock.status).unwrap())
                .header(
                    "content-type",
                    if mock.sse {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                );
            if let Some(location) = mock.location {
                response = response.header("location", location);
            }
            if mock.sse {
                // Deliberately split JSON tokens and multibyte characters across body frames.
                let frames: Vec<_> = mock
                    .data
                    .chunks(7)
                    .map(|b| Ok::<_, std::convert::Infallible>(bytes::Bytes::copy_from_slice(b)))
                    .collect();
                response
                    .body(Body::from_stream(futures::stream::iter(frames)))
                    .unwrap()
            } else {
                response.body(Body::from(mock.data)).unwrap()
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new()
            .fallback(any(handler))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { base, state, task }
    }
    fn count(&self) -> usize {
        self.state.requests.lock().unwrap().len()
    }
}
impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn target_input(protocol: ValidationProtocol) -> TargetInput {
    TargetInput {
        app_id: match protocol {
            ValidationProtocol::OpenaiChat => "opencode",
            ValidationProtocol::OpenaiResponses => "codex",
            ValidationProtocol::Anthropic => "claude",
            ValidationProtocol::Gemini => "gemini",
        }
        .into(),
        provider_id: "test-provider-a".into(),
        model: "test-model".into(),
        protocol: Some(protocol),
    }
}

#[tokio::test]
async fn model_discovery_uses_explicit_key_for_each_protocol_without_a_requested_model() {
    for protocol in [
        ValidationProtocol::OpenaiChat,
        ValidationProtocol::OpenaiResponses,
        ValidationProtocol::Anthropic,
        ValidationProtocol::Gemini,
    ] {
        let payload = if protocol == ValidationProtocol::Gemini {
            json!({"models":[{"name":"models/synthetic-model"}]})
        } else {
            json!({"data":[{"id":"synthetic-model"}]})
        };
        let server = MockServer::new(vec![MockResponse::json(200, payload)]).await;
        let mut input = target_input(protocol);
        input.model.clear();
        let member = provider(&input, &server.base, FAKE_KEY);
        let db = Database::memory().unwrap();
        db.save_provider(&input.app_id, &member).unwrap();
        assert!(PinnedTarget::resolve(&db, &input, ValidationMode::Direct).is_err());
        let before = serde_json::to_value(
            db.get_provider_by_id(&input.provider_id, &input.app_id)
                .unwrap(),
        )
        .unwrap();
        let models = fetch_validation_models(&db, input.clone()).await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "synthetic-model");
        assert_eq!(server.count(), 1);
        let requests = server.state.requests.lock().unwrap();
        let (path, headers, body) = &requests[0];
        assert!(body.is_null());
        match protocol {
            ValidationProtocol::Gemini => {
                assert_eq!(path, "/v1beta/models");
                assert_eq!(headers["x-goog-api-key"], FAKE_KEY);
            }
            ValidationProtocol::Anthropic => {
                assert_eq!(path, "/v1/models");
                assert_eq!(headers["x-api-key"], FAKE_KEY);
                assert_eq!(headers["anthropic-version"], "2023-06-01");
            }
            _ => {
                assert_eq!(path, "/v1/models");
                assert_eq!(headers["authorization"], format!("Bearer {FAKE_KEY}"));
            }
        }
        assert_eq!(
            before,
            serde_json::to_value(
                db.get_provider_by_id(&input.provider_id, &input.app_id)
                    .unwrap()
            )
            .unwrap()
        );
    }
}

#[tokio::test]
async fn model_discovery_handles_pagination_deduplicates_and_redacts_metadata() {
    let server = MockServer::new(vec![
        MockResponse::json(
            200,
            json!({"data":[{"id":"beta"}],"has_more":true,"last_id":"beta"}),
        ),
        MockResponse::json(
            200,
            json!({"data":[{"id":"alpha","owned_by":FAKE_KEY},{"id":"beta"}],"has_more":false}),
        ),
    ])
    .await;
    let input = target_input(ValidationProtocol::Anthropic);
    let db = Database::memory().unwrap();
    db.save_provider(&input.app_id, &provider(&input, &server.base, FAKE_KEY))
        .unwrap();
    let models = fetch_validation_models(&db, input).await.unwrap();
    assert_eq!(
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );
    assert!(!serde_json::to_string(&models).unwrap().contains(FAKE_KEY));
    assert_eq!(
        server.state.requests.lock().unwrap()[1].0,
        "/v1/models?after_id=beta"
    );
}

#[tokio::test]
async fn model_discovery_handles_gemini_versions_and_page_tokens() {
    let server = MockServer::new(vec![
        MockResponse::json(
            200,
            json!({"models":[{"name":"models/gemini-a"}],"nextPageToken":"next"}),
        ),
        MockResponse::json(200, json!({"models":[{"name":"models/gemini-b"}]})),
    ])
    .await;
    let input = target_input(ValidationProtocol::Gemini);
    let db = Database::memory().unwrap();
    db.save_provider(
        &input.app_id,
        &provider(&input, &format!("{}/v1beta", server.base), FAKE_KEY),
    )
    .unwrap();
    let models = fetch_validation_models(&db, input).await.unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(
        server.state.requests.lock().unwrap()[1].0,
        "/v1beta/models?pageToken=next"
    );
}

#[tokio::test]
async fn model_discovery_never_retries_auth_errors_follows_redirects_or_exposes_error_bodies() {
    let destination = MockServer::new(vec![]).await;
    for status in [401, 403, 503, 302] {
        let mut reply = MockResponse::json(status, json!({"error":FAKE_KEY}));
        reply.location = Some(destination.base.clone());
        let server = MockServer::new(vec![reply]).await;
        let input = target_input(ValidationProtocol::Anthropic);
        let db = Database::memory().unwrap();
        db.save_provider(&input.app_id, &provider(&input, &server.base, FAKE_KEY))
            .unwrap();
        let error = fetch_validation_models(&db, input)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(&format!("HTTP {status}")));
        assert!(!error.contains(FAKE_KEY));
        assert_eq!(server.count(), 1);
        assert_eq!(destination.count(), 0);
    }
}

#[tokio::test]
async fn model_discovery_only_tries_compatible_paths_after_404() {
    let server = MockServer::new(vec![
        MockResponse::json(404, json!({"error":"not found"})),
        MockResponse::json(200, json!({"data":[{"id":"available"}]})),
    ])
    .await;
    let input = target_input(ValidationProtocol::Anthropic);
    let db = Database::memory().unwrap();
    db.save_provider(
        &input.app_id,
        &provider(&input, &format!("{}/anthropic", server.base), FAKE_KEY),
    )
    .unwrap();
    assert_eq!(
        fetch_validation_models(&db, input).await.unwrap()[0].id,
        "available"
    );
    let requests = server.state.requests.lock().unwrap();
    assert_eq!(requests[0].0, "/anthropic/v1/models");
    assert_eq!(requests[1].0, "/v1/models");
    assert!(requests
        .iter()
        .all(|(_, headers, _)| headers["x-api-key"] == FAKE_KEY));
}

#[tokio::test]
async fn model_discovery_rejects_dynamic_keys_and_invalid_or_oversized_responses() {
    let server = MockServer::new(vec![]).await;
    let input = target_input(ValidationProtocol::Anthropic);
    let db = Database::memory().unwrap();
    for key in ["$ENV_KEY", "sk-ant-oat-example"] {
        db.save_provider(&input.app_id, &provider(&input, &server.base, key))
            .unwrap();
        assert!(fetch_validation_models(&db, input.clone()).await.is_err());
    }
    assert_eq!(server.count(), 0);
    for payload in [
        json!({"unexpected":[]}),
        json!({"data":[{"id":FAKE_KEY}]}),
        json!({"data":[],"padding":"x".repeat(2_097_153)}),
    ] {
        let server = MockServer::new(vec![MockResponse::json(200, payload)]).await;
        db.save_provider(&input.app_id, &provider(&input, &server.base, FAKE_KEY))
            .unwrap();
        let error = fetch_validation_models(&db, input.clone())
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains(FAKE_KEY));
        assert_eq!(server.count(), 1);
    }
}

fn provider(input: &TargetInput, base: &str, key: &str) -> Provider {
    let config = match input.app_id.as_str() {
        "opencode" => json!({"options":{"baseURL":base,"apiKey":key}}),
        "codex" => json!({"base_url":base,"auth":{"OPENAI_API_KEY":key}}),
        "gemini" => json!({"env":{"GOOGLE_GEMINI_BASE_URL":base,"GEMINI_API_KEY":key}}),
        _ => json!({"env":{"ANTHROPIC_BASE_URL":base,"ANTHROPIC_API_KEY":key}}),
    };
    Provider::with_id(
        input.provider_id.clone(),
        "Mock supplier".into(),
        config,
        None,
    )
}

fn pinned(protocol: ValidationProtocol, base: &str) -> PinnedTarget {
    let input = target_input(protocol);
    PinnedTarget::from_provider(
        input.app_id.parse().unwrap(),
        provider(&input, base, FAKE_KEY),
        &input,
        ValidationMode::Direct,
    )
    .unwrap()
}

fn response(protocol: ValidationProtocol, text: &str) -> Value {
    match protocol {
        ValidationProtocol::OpenaiChat => {
            json!({"model":"test-model","choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}],"usage":{"prompt_tokens":8,"completion_tokens":4}})
        }
        ValidationProtocol::OpenaiResponses => {
            json!({"model":"test-model","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}],"usage":{"input_tokens":8,"output_tokens":4}})
        }
        ValidationProtocol::Anthropic => {
            json!({"type":"message","model":"test-model","role":"assistant","content":[{"type":"text","text":text}],"stop_reason":"end_turn","usage":{"input_tokens":8,"output_tokens":4}})
        }
        ValidationProtocol::Gemini => {
            json!({"modelVersion":"test-model","candidates":[{"content":{"role":"model","parts":[{"text":text}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":8,"candidatesTokenCount":4}})
        }
    }
}

fn executor(max: u32) -> Executor {
    Executor::new(
        ValidationMode::Direct,
        None,
        Cancellation::default(),
        max,
        15,
    )
    .unwrap()
}

async fn probe(
    probe: Probe,
    protocol: ValidationProtocol,
    responses: Vec<MockResponse>,
) -> (ProbeResult, MockServer) {
    let server = MockServer::new(responses).await;
    let target = pinned(protocol, &server.base);
    let (max_requests, max_tokens) = probes::budget(probe, 2, false);
    let mut executor = executor(max_requests);
    let result = super::probes::run(probe, &mut executor, &target, None, 2, "fixture").await;
    let request_limit = match probe {
        Probe::Thinking | Probe::Signature | Probe::CrossSignature => protocol::THINKING_TOKENS,
        Probe::OutputLimit => protocol::LIMIT_TOKENS,
        Probe::Comparison => protocol::COMPARISON_TOKENS,
        _ => protocol::BASIC_TOKENS,
    };
    {
        let records = server.state.requests.lock().unwrap();
        assert_eq!(records.len() as u32, result.request_count);
        assert!(result.request_count <= max_requests);
        let mut tokens = 0;
        for (_, _, body) in records.iter() {
            let limit = output_token_limit(protocol, body);
            assert!(limit > 0 && limit <= u64::from(request_limit));
            assert!(body.get("n").and_then(Value::as_u64).unwrap_or(1) == 1);
            assert!(
                body.pointer("/generationConfig/candidateCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    == 1
            );
            tokens += limit;
        }
        assert!(tokens <= u64::from(max_tokens));
    }
    (result, server)
}

fn output_token_limit(protocol: ValidationProtocol, body: &Value) -> u64 {
    let path = match protocol {
        ValidationProtocol::OpenaiChat => "/max_completion_tokens",
        ValidationProtocol::OpenaiResponses => "/max_output_tokens",
        ValidationProtocol::Anthropic => "/max_tokens",
        ValidationProtocol::Gemini => "/generationConfig/maxOutputTokens",
    };
    body.pointer(path)
        .and_then(Value::as_u64)
        .expect("explicit request output limit")
}

#[tokio::test]
async fn calls_and_static_auth_work_for_all_four_protocols() {
    for p in [
        ValidationProtocol::OpenaiChat,
        ValidationProtocol::OpenaiResponses,
        ValidationProtocol::Anthropic,
        ValidationProtocol::Gemini,
    ] {
        let (result, server) = probe(
            Probe::Call,
            p,
            vec![MockResponse::json(200, response(p, "CCS_OK"))],
        )
        .await;
        assert_eq!(result.status, ProbeStatus::Passed);
        let records = server.state.requests.lock().unwrap();
        assert_eq!(records.len(), 1);
        let (uri, headers, body) = &records[0];
        assert!(!uri.contains(FAKE_KEY));
        match p {
            ValidationProtocol::Anthropic => {
                assert_eq!(headers["x-api-key"], FAKE_KEY);
                assert!(!headers.contains_key("authorization"));
                assert_eq!(uri, "/v1/messages");
            }
            ValidationProtocol::Gemini => {
                assert_eq!(headers["x-goog-api-key"], FAKE_KEY);
                assert!(!body.as_object().unwrap().contains_key("stream"));
                assert_eq!(uri, "/v1beta/models/test-model:generateContent");
            }
            _ => assert_eq!(headers["authorization"], format!("Bearer {FAKE_KEY}")),
        }
        assert!(!serde_json::to_string(&result).unwrap().contains(FAKE_KEY));
    }
}

#[test]
fn sse_ignores_heartbeats_and_role_only_and_preserves_split_utf8() {
    let mut decoder = SseDecoder::new(ValidationProtocol::OpenaiChat);
    decoder
        .feed(
            b": heartbeat\n\ndata: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            1,
        )
        .unwrap();
    assert_eq!(decoder.first_content_ms, None);
    let event = "data: {\"choices\":[{\"delta\":{\"content\":\"中文CCS_OK\"}}]}\r\n\r\n";
    for byte in event.as_bytes() {
        decoder.feed(&[*byte], 25).unwrap();
    }
    assert_eq!(decoder.first_content_ms, Some(25));
    decoder
        .feed(
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            30,
        )
        .unwrap();
    assert!(decoder.complete());
    assert_eq!(decoder.observation.text, "中文CCS_OK");
}

#[tokio::test]
async fn complete_and_broken_streams_are_distinguished_for_all_protocols() {
    let streams = [
        (ValidationProtocol::OpenaiChat, "data: {\"choices\":[{\"delta\":{\"content\":\"CCS_OK\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"),
        (ValidationProtocol::OpenaiResponses, "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"CCS_OK\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"),
        (ValidationProtocol::Anthropic, "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"CCS_OK\"}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n"),
        (ValidationProtocol::Gemini, "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"CCS_OK\"}]},\"finishReason\":\"STOP\"}]}\n\n"),
    ];
    for (p, text) in streams {
        let (result, _) = probe(Probe::Stream, p, vec![MockResponse::sse(text)]).await;
        assert_eq!(
            result.status,
            ProbeStatus::Passed,
            "{p:?}: {}",
            result.summary
        );
        let (broken, _) = probe(Probe::Stream, p, vec![MockResponse::sse(text.trim_end())]).await;
        assert_eq!(broken.status, ProbeStatus::Failed, "{p:?}");
    }
    let (heartbeat, _) = probe(Probe::Stream,ValidationProtocol::OpenaiChat,vec![MockResponse::sse(": ping\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")]).await;
    assert_eq!(heartbeat.status, ProbeStatus::Failed);
}

#[tokio::test]
async fn a_json_response_does_not_pass_stream_validation() {
    let p = ValidationProtocol::OpenaiChat;
    let (result, _) = probe(
        Probe::Stream,
        p,
        vec![MockResponse::json(200, response(p, "CCS_OK"))],
    )
    .await;
    assert_eq!(result.status, ProbeStatus::Failed);
}

#[tokio::test]
async fn structured_output_requires_exact_fields_and_semantic_values() {
    for p in [
        ValidationProtocol::OpenaiChat,
        ValidationProtocol::OpenaiResponses,
        ValidationProtocol::Anthropic,
        ValidationProtocol::Gemini,
    ] {
        let (passed, _) = probe(
            Probe::Structured,
            p,
            vec![MockResponse::json(
                200,
                response(p, r#"{"answer":42,"label":"ccs"}"#),
            )],
        )
        .await;
        assert_eq!(passed.status, ProbeStatus::Passed);
        let (failed, _) = probe(
            Probe::Structured,
            p,
            vec![MockResponse::json(
                200,
                response(p, r#"{"answer":41,"label":"ccs"}"#),
            )],
        )
        .await;
        assert_eq!(failed.status, ProbeStatus::Failed);
    }
}

#[tokio::test]
async fn tool_calls_only_execute_the_fixed_synthetic_function_and_roundtrip() {
    for p in [
        ValidationProtocol::OpenaiChat,
        ValidationProtocol::OpenaiResponses,
        ValidationProtocol::Anthropic,
        ValidationProtocol::Gemini,
    ] {
        let tool = match p {
            ValidationProtocol::OpenaiChat => {
                json!({"choices":[{"message":{"tool_calls":[{"id":"c1","type":"function","function":{"name":"ccs_add","arguments":"{\"a\":19,\"b\":23}"}}]}}]})
            }
            ValidationProtocol::OpenaiResponses => {
                json!({"output":[{"type":"function_call","call_id":"c1","name":"ccs_add","arguments":"{\"a\":19,\"b\":23}"}]})
            }
            ValidationProtocol::Anthropic => {
                json!({"content":[{"type":"tool_use","id":"c1","name":"ccs_add","input":{"a":19,"b":23}}]})
            }
            ValidationProtocol::Gemini => {
                json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"ccs_add","args":{"a":19,"b":23}}}]}}]})
            }
        };
        let (result, server) = probe(
            Probe::Tools,
            p,
            vec![
                MockResponse::json(200, tool.clone()),
                MockResponse::json(200, response(p, "42")),
            ],
        )
        .await;
        assert_eq!(result.status, ProbeStatus::Passed);
        assert_eq!(server.count(), 2);
        let hostile = tool.to_string().replace("ccs_add", "execute_shell");
        let (failed, server) = probe(
            Probe::Tools,
            p,
            vec![MockResponse::json(
                200,
                serde_json::from_str(&hostile).unwrap(),
            )],
        )
        .await;
        assert_eq!(failed.status, ProbeStatus::Failed);
        assert_eq!(server.count(), 1);
    }
}

#[tokio::test]
async fn image_test_sends_a_valid_synthetic_png_and_checks_its_pixels() {
    use base64::Engine;
    use std::io::Read;
    let color = "fixture"
        .bytes()
        .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32))
        % 3;
    let (expected, rgb) = match color {
        0 => ("RED", [255, 0, 0]),
        1 => ("GREEN", [0, 255, 0]),
        _ => ("BLUE", [0, 0, 255]),
    };
    for p in [
        ValidationProtocol::OpenaiChat,
        ValidationProtocol::OpenaiResponses,
        ValidationProtocol::Anthropic,
        ValidationProtocol::Gemini,
    ] {
        let (result, server) = probe(
            Probe::Image,
            p,
            vec![MockResponse::json(200, response(p, expected))],
        )
        .await;
        assert_eq!(result.status, ProbeStatus::Passed);
        let records = server.state.requests.lock().unwrap();
        let body = &records[0].2;
        let encoded = match p {
            ValidationProtocol::OpenaiChat => {
                body.pointer("/messages/0/content/1/image_url/url")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .split_once(',')
                    .unwrap()
                    .1
            }
            ValidationProtocol::OpenaiResponses => {
                body.pointer("/input/0/content/1/image_url")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .split_once(',')
                    .unwrap()
                    .1
            }
            ValidationProtocol::Anthropic => body
                .pointer("/messages/0/content/1/source/data")
                .unwrap()
                .as_str()
                .unwrap(),
            ValidationProtocol::Gemini => body
                .pointer("/contents/0/parts/1/inlineData/data")
                .unwrap()
                .as_str()
                .unwrap(),
        };
        let png = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let idat = png.windows(4).position(|w| w == b"IDAT").unwrap();
        let len = u32::from_be_bytes(png[idat - 4..idat].try_into().unwrap()) as usize;
        let mut decoded = Vec::new();
        flate2::read::ZlibDecoder::new(&png[idat + 4..idat + 4 + len])
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(&decoded[1..4], &rgb);
    }
}

#[tokio::test]
async fn cache_requires_counters_and_thinking_requires_evidence() {
    let p = ValidationProtocol::Anthropic;
    let mut first = response(p, "CCS_OK");
    first["usage"]["cache_creation_input_tokens"] = json!(8000);
    let mut second = response(p, "CCS_OK");
    second["usage"]["cache_read_input_tokens"] = json!(8000);
    let (passed, server) = probe(
        Probe::Cache,
        p,
        vec![
            MockResponse::json(200, first),
            MockResponse::json(200, second),
        ],
    )
    .await;
    assert_eq!(passed.status, ProbeStatus::Passed);
    let requests = server.state.requests.lock().unwrap();
    assert_eq!(requests[0].2, requests[1].2);
    drop(requests);
    let (missing, _) = probe(
        Probe::Cache,
        p,
        vec![
            MockResponse::json(200, response(p, "CCS_OK")),
            MockResponse::json(200, response(p, "CCS_OK")),
        ],
    )
    .await;
    assert_eq!(missing.status, ProbeStatus::Inconclusive);
    let (thinking, _) = probe(
        Probe::Thinking,
        ValidationProtocol::OpenaiChat,
        vec![MockResponse::json(
            200,
            response(ValidationProtocol::OpenaiChat, "42"),
        )],
    )
    .await;
    assert_eq!(thinking.status, ProbeStatus::Inconclusive);
}

#[tokio::test]
async fn output_limit_never_infers_tokens_from_character_count() {
    let p = ValidationProtocol::OpenaiChat;
    let mut full = response(p, "1 2 3 4 5");
    full["choices"][0]["finish_reason"] = json!("length");
    full["usage"]["completion_tokens"] = json!(16);
    let (passed, _) = probe(
        Probe::OutputLimit,
        p,
        vec![MockResponse::json(200, full.clone())],
    )
    .await;
    assert_eq!(passed.status, ProbeStatus::Passed);
    full["usage"]["completion_tokens"] = json!(17);
    let (failed, _) = probe(
        Probe::OutputLimit,
        p,
        vec![MockResponse::json(200, full.clone())],
    )
    .await;
    assert_eq!(failed.status, ProbeStatus::Failed);
    full.as_object_mut().unwrap().remove("usage");
    let (unknown, _) = probe(Probe::OutputLimit, p, vec![MockResponse::json(200, full)]).await;
    assert_eq!(unknown.status, ProbeStatus::Inconclusive);
}

fn signed_reply() -> Value {
    let mut value = response(ValidationProtocol::Anthropic, "42");
    value["content"].as_array_mut().unwrap().insert(0,json!({"type":"thinking","thinking":"synthetic arithmetic only","signature":"c3ludGhldGljLXNpZ25hdHVyZQ=="}));
    value
}

#[tokio::test]
async fn signature_requires_successful_control_and_specific_rejection() {
    let p = ValidationProtocol::Anthropic;
    for (message, expected) in [
        ("Invalid signature in thinking block", ProbeStatus::Passed),
        ("Unknown model", ProbeStatus::Inconclusive),
        ("signature: unknown field", ProbeStatus::Inconclusive),
    ] {
        let (result, server) = probe(
            Probe::Signature,
            p,
            vec![
                MockResponse::json(200, signed_reply()),
                MockResponse::json(200, response(p, "CCS_OK")),
                MockResponse::json(400, json!({"error":{"message":message}})),
            ],
        )
        .await;
        assert_eq!(result.status, expected, "{message}");
        assert_eq!(server.count(), 3);
        let history = serde_json::to_string(&result).unwrap();
        assert!(!history.contains("c3ludG"));
        assert!(!history.contains("synthetic arithmetic only"));
    }
    let (failed_control, server) = probe(
        Probe::Signature,
        p,
        vec![
            MockResponse::json(200, signed_reply()),
            MockResponse::json(400, json!({"error":{"message":"invalid signature"}})),
        ],
    )
    .await;
    assert_eq!(failed_control.status, ProbeStatus::Inconclusive);
    assert_eq!(server.count(), 2);
    let (accepted, _) = probe(
        Probe::Signature,
        p,
        vec![
            MockResponse::json(200, signed_reply()),
            MockResponse::json(200, response(p, "CCS_OK")),
            MockResponse::json(200, response(p, "CCS_OK")),
        ],
    )
    .await;
    assert_eq!(accepted.status, ProbeStatus::Failed);
}

#[tokio::test]
async fn cross_signature_checks_both_normal_controls() {
    let p = ValidationProtocol::Anthropic;
    let a = MockServer::new(vec![
        MockResponse::json(200, signed_reply()),
        MockResponse::json(200, response(p, "CCS_OK")),
    ])
    .await;
    let b = MockServer::new(vec![
        MockResponse::json(200, signed_reply()),
        MockResponse::json(200, response(p, "CCS_OK")),
        MockResponse::json(200, response(p, "CCS_OK")),
    ])
    .await;
    let t = pinned(p, &a.base);
    let other = pinned(p, &b.base);
    let result = probes::run(
        Probe::CrossSignature,
        &mut executor(5),
        &t,
        Some(&other),
        2,
        "fixture",
    )
    .await;
    assert_eq!(result.status, ProbeStatus::Passed);
    assert_eq!((a.count(), b.count()), (2, 3));
    assert_eq!(result.request_count, 5);
}

#[tokio::test]
async fn comparison_runs_repeated_controlled_tasks_without_identity_claims() {
    let p = ValidationProtocol::OpenaiChat;
    let mock_responses = || {
        ["323", "-2,0,4,9", "amber", "323", "-2,0,4,9", "amber"]
            .into_iter()
            .map(|s| MockResponse::json(200, response(p, s)))
            .collect()
    };
    let a = MockServer::new(mock_responses()).await;
    let b = MockServer::new(mock_responses()).await;
    let result = probes::run(
        Probe::Comparison,
        &mut executor(12),
        &pinned(p, &a.base),
        Some(&pinned(p, &b.base)),
        2,
        "fixture",
    )
    .await;
    assert_eq!(result.status, ProbeStatus::Passed);
    assert_eq!(result.request_count, 12);
    assert_eq!(
        result
            .evidence
            .iter()
            .filter(|e| e.label.starts_with("sample."))
            .count(),
        12
    );
    for server in [&a, &b] {
        for (_, _, body) in server.state.requests.lock().unwrap().iter() {
            assert_eq!(body["temperature"], 0);
            assert_eq!(body["max_completion_tokens"], 128);
        }
    }
    assert!(result.summary.contains("不能据此"));
}

#[test]
fn target_resolution_covers_ten_apps_without_reading_environment_or_scripts() {
    let base = "https://example.invalid/v1";
    for app in AppType::all() {
        let config = match app {
            AppType::Claude | AppType::ClaudeDesktop => {
                json!({"env":{"ANTHROPIC_BASE_URL":base,"ANTHROPIC_API_KEY":FAKE_KEY}})
            }
            AppType::Codex => {
                json!({"auth":{"OPENAI_API_KEY":FAKE_KEY},"config":format!("model_provider = 'custom'\n[model_providers.custom]\nbase_url = '{base}'\nwire_api = 'responses'")})
            }
            AppType::GrokBuild => {
                json!({"config":format!("[models]\ndefault = 'custom'\n[model.custom]\nbase_url = '{base}'\napi_key = '{FAKE_KEY}'\napi_backend = 'responses'")})
            }
            AppType::Gemini => {
                json!({"env":{"GOOGLE_GEMINI_BASE_URL":base,"GEMINI_API_KEY":FAKE_KEY}})
            }
            AppType::OpenCode => json!({"options":{"baseURL":base,"apiKey":FAKE_KEY}}),
            AppType::Hermes => json!({"base_url":base,"api_key":FAKE_KEY}),
            _ => json!({"baseUrl":base,"apiKey":FAKE_KEY}),
        };
        let input = TargetInput {
            app_id: app.as_str().into(),
            provider_id: "mock".into(),
            model: "test-model".into(),
            protocol: None,
        };
        let target = PinnedTarget::from_provider(
            app,
            Provider::with_id("mock".into(), "Mock".into(), config, None),
            &input,
            ValidationMode::Direct,
        )
        .unwrap();
        assert_eq!(target.key, FAKE_KEY);
        assert!(!serde_json::to_string(&target.summary)
            .unwrap()
            .contains(FAKE_KEY));
    }
    let input = TargetInput {
        app_id: "grokbuild".into(),
        provider_id: "mock".into(),
        model: "test-model".into(),
        protocol: None,
    };
    let provider = Provider::with_id(
        "mock".into(),
        "Mock".into(),
        json!({"config":"[models]\ndefault='custom'\n[model.custom]\nbase_url='https://example.invalid'\nenv_key='SOME_SECRET_KEY'\napi_backend='responses'"}),
        None,
    );
    assert!(PinnedTarget::from_provider(
        AppType::GrokBuild,
        provider,
        &input,
        ValidationMode::Direct
    )
    .is_err());
}

#[test]
fn rejects_oauth_dynamic_credentials_and_secret_bearing_urls() {
    let input = target_input(ValidationProtocol::OpenaiChat);
    for key in [
        "ya29.oauth",
        "!run-script",
        "${SECRET}",
        "{env:SECRET}",
        "sk-ant-oat-abc",
        "PROXY_MANAGED",
    ] {
        assert!(PinnedTarget::from_provider(
            AppType::OpenCode,
            provider(&input, "https://example.invalid", key),
            &input,
            ValidationMode::Direct
        )
        .is_err());
    }
    for endpoint in [
        "https://user:password@example.invalid",
        "https://example.invalid?key=secret",
        "file:///tmp/test",
        "https://chatgpt.com/backend-api/codex",
    ] {
        assert!(PinnedTarget::from_provider(
            AppType::OpenCode,
            provider(&input, endpoint, FAKE_KEY),
            &input,
            ValidationMode::Direct
        )
        .is_err());
    }
    let mut oauth = provider(&input, "https://example.invalid", FAKE_KEY);
    oauth.meta = Some(ProviderMeta {
        provider_type: Some("codex_oauth".into()),
        ..Default::default()
    });
    assert!(
        PinnedTarget::from_provider(AppType::OpenCode, oauth, &input, ValidationMode::Direct)
            .is_err()
    );
}

#[tokio::test]
async fn no_retry_redirect_or_failover_and_no_secret_in_errors() {
    let p = ValidationProtocol::OpenaiChat;
    let fallback = MockServer::new(vec![MockResponse::json(200, response(p, "CCS_OK"))]).await;
    let mut redirect = MockResponse::json(302, json!({}));
    redirect.location = Some(format!("{}/v1/chat/completions", fallback.base));
    let (redirected, primary) = probe(Probe::Call, p, vec![redirect]).await;
    assert_eq!(redirected.status, ProbeStatus::Failed);
    assert_eq!((primary.count(), fallback.count()), (1, 0));
    let (failure, primary) = probe(
        Probe::Call,
        p,
        vec![
            MockResponse::json(
                401,
                json!({"error":{"message":format!("bad key {FAKE_KEY}")}}),
            ),
            MockResponse::json(200, response(p, "CCS_OK")),
        ],
    )
    .await;
    assert_eq!(failure.status, ProbeStatus::Failed);
    assert_eq!(primary.count(), 1);
    assert!(!serde_json::to_string(&failure).unwrap().contains(FAKE_KEY));
}

#[tokio::test]
async fn transport_enforces_size_time_and_request_budgets() {
    let p = ValidationProtocol::OpenaiChat;
    let oversized = MockResponse {
        status: 200,
        data: vec![b' '; 1_048_577],
        sse: false,
        delay: Duration::ZERO,
        location: None,
    };
    let server = MockServer::new(vec![oversized]).await;
    let target = pinned(p, &server.base);
    let body = protocol::request(p, "test-model", protocol::CALL_PROMPT, 256, false);
    assert!(matches!(
        executor(1).send(&target, body.clone()).await,
        Err(RequestFailure::TooLarge)
    ));
    let mut timed =
        Executor::new(ValidationMode::Direct, None, Cancellation::default(), 1, 0).unwrap();
    assert!(matches!(
        timed.send(&target, body.clone()).await,
        Err(RequestFailure::Timeout)
    ));
    let mut zero = executor(0);
    assert!(matches!(
        zero.send(&target, body).await,
        Err(RequestFailure::Budget)
    ));
    assert_eq!(server.count(), 1);
}

async fn completed(runtime: &Runtime, db: &Arc<Database>, id: &str) -> ValidationRun {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let run = runtime.get(db, id).unwrap();
            if run.status != RunStatus::Running {
                break run;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("mock validation finishes")
}

fn prepare_request(input: TargetInput, probes: Vec<Probe>) -> PrepareRequest {
    PrepareRequest {
        target: input,
        mode: ValidationMode::Direct,
        probes,
        comparison_target: None,
        repeat_count: None,
    }
}

#[tokio::test]
async fn prepare_is_network_free_start_is_idempotent_and_history_is_sanitized() {
    let p = ValidationProtocol::OpenaiChat;
    let server = MockServer::new(vec![MockResponse::json(200, response(p, "CCS_OK"))]).await;
    let db = Arc::new(Database::memory().unwrap());
    let input = target_input(p);
    let config = provider(&input, &server.base, FAKE_KEY);
    db.save_provider(&input.app_id, &config).unwrap();
    let runtime = Arc::new(Runtime::default());
    let plan = runtime
        .prepare(&db, prepare_request(input.clone(), vec![Probe::Call]))
        .unwrap();
    assert_eq!(server.count(), 0);
    assert_eq!(plan.max_requests, 1);
    assert_eq!(plan.max_output_tokens, 256);
    assert!(plan.estimated_cost_usd.is_none());
    assert!(!serde_json::to_string(&plan).unwrap().contains(FAKE_KEY));
    let run = runtime.start(db.clone(), None, &plan.id).unwrap();
    let retry = runtime.start(db.clone(), None, &plan.id).unwrap();
    assert_eq!(run.id, retry.id);
    let done = completed(&runtime, &db, &run.id).await;
    assert_eq!(done.status, RunStatus::Completed);
    assert_eq!(done.results[0].status, ProbeStatus::Passed);
    assert_eq!(server.count(), 1);
    let history = runtime
        .list(&db, Some(&input.app_id), Some(&input.provider_id), 10)
        .unwrap();
    assert_eq!(history.len(), 1);
    assert!(!serde_json::to_string(&history).unwrap().contains(FAKE_KEY));
    assert_eq!(
        db.get_provider_by_id(&input.provider_id, &input.app_id)
            .unwrap()
            .unwrap()
            .settings_config,
        config.settings_config
    );
}

#[tokio::test]
async fn changed_provider_rejects_stale_preview_without_using_new_or_other_key() {
    let server = MockServer::new(vec![]).await;
    let p = ValidationProtocol::OpenaiChat;
    let db = Arc::new(Database::memory().unwrap());
    let input = target_input(p);
    db.save_provider(&input.app_id, &provider(&input, &server.base, FAKE_KEY))
        .unwrap();
    let runtime = Arc::new(Runtime::default());
    let plan = runtime
        .prepare(&db, prepare_request(input.clone(), vec![Probe::Call]))
        .unwrap();
    db.save_provider(
        &input.app_id,
        &provider(&input, &server.base, "different-fake-key"),
    )
    .unwrap();
    assert!(runtime.start(db.clone(), None, &plan.id).is_err());
    assert_eq!(server.count(), 0);
}

#[tokio::test]
async fn cancellation_stops_the_current_response_and_all_following_probes() {
    let p = ValidationProtocol::OpenaiChat;
    let mut slow = MockResponse::json(200, response(p, "CCS_OK"));
    slow.delay = Duration::from_secs(10);
    let server = MockServer::new(vec![slow]).await;
    let db = Arc::new(Database::memory().unwrap());
    let input = target_input(p);
    db.save_provider(&input.app_id, &provider(&input, &server.base, FAKE_KEY))
        .unwrap();
    let runtime = Arc::new(Runtime::default());
    let plan = runtime
        .prepare(
            &db,
            prepare_request(input, vec![Probe::Call, Probe::Stream]),
        )
        .unwrap();
    let run = runtime.start(db.clone(), None, &plan.id).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.count() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(runtime.cancel(&db, &run.id).unwrap());
    let done = completed(&runtime, &db, &run.id).await;
    assert_eq!(done.status, RunStatus::Cancelled);
    assert_eq!(server.count(), 1);
    assert_eq!(done.results[1].request_count, 0);
    assert_eq!(done.results[1].status, ProbeStatus::NotTested);
    assert!(!runtime.cancel(&db, &run.id).unwrap());
}

#[tokio::test]
async fn persisted_running_record_is_interrupted_after_restart_and_never_replayed() {
    let server = MockServer::new(vec![]).await;
    let p = ValidationProtocol::OpenaiChat;
    let db = Arc::new(Database::memory().unwrap());
    let input = target_input(p);
    db.save_provider(&input.app_id, &provider(&input, &server.base, FAKE_KEY))
        .unwrap();
    let runtime = Arc::new(Runtime::default());
    let plan = runtime
        .prepare(&db, prepare_request(input, vec![Probe::Call]))
        .unwrap();
    let run = ValidationRun {
        id: "prior-run".into(),
        plan,
        status: RunStatus::Running,
        started_at: chrono::Utc::now().to_rfc3339(),
        finished_at: None,
        results: vec![],
    };
    db.save_model_validation_run(&run).unwrap();
    let recovered = Arc::new(Runtime::default())
        .start(db.clone(), None, &run.plan.id)
        .unwrap();
    assert_eq!(recovered.status, RunStatus::Interrupted);
    assert_eq!(server.count(), 0);
}

#[test]
fn wire_dtos_use_camel_case_and_protocols_remain_snake_case() {
    let request = prepare_request(
        target_input(ValidationProtocol::OpenaiResponses),
        vec![Probe::OutputLimit],
    );
    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(value["target"]["appId"], "codex");
    assert_eq!(value["target"]["providerId"], "test-provider-a");
    assert_eq!(value["target"]["protocol"], "openai_responses");
    assert_eq!(value["probes"][0], "output_limit");
    assert!(value.get("repeatCount").is_some());
    assert!(value.get("repeat_count").is_none());
}

#[test]
fn model_level_pi_config_wins_and_ccs_protocol_override_is_rejected() {
    let input = TargetInput {
        app_id: "pi".into(),
        provider_id: "p".into(),
        model: "chosen".into(),
        protocol: None,
    };
    let p = Provider::with_id(
        "p".into(),
        "Mock".into(),
        json!({"baseUrl":"https://default.invalid/v1","apiKey":FAKE_KEY,"api":"openai-completions","models":[{"id":"chosen","baseUrl":"https://chosen.invalid","api":"anthropic-messages"}]}),
        None,
    );
    let pinned =
        PinnedTarget::from_provider(AppType::Pi, p, &input, ValidationMode::Direct).unwrap();
    assert_eq!(pinned.summary.endpoint, "https://chosen.invalid");
    assert_eq!(pinned.summary.protocol, ValidationProtocol::Anthropic);
    let mut input = target_input(ValidationProtocol::Anthropic);
    input.protocol = Some(ValidationProtocol::OpenaiChat);
    assert!(PinnedTarget::from_provider(
        AppType::Claude,
        provider(&input, "https://example.invalid", FAKE_KEY),
        &input,
        ValidationMode::Ccs
    )
    .is_err());
}

#[test]
fn url_builder_does_not_silently_replace_explicit_full_endpoints() {
    for (protocol, path, expected) in [
        (
            ValidationProtocol::OpenaiResponses,
            "/responses",
            "https://example.invalid/custom/responses",
        ),
        (
            ValidationProtocol::OpenaiChat,
            "/v1/chat/completions",
            "https://example.invalid/custom/chat/completions",
        ),
    ] {
        assert_eq!(
            transport::direct_url("https://example.invalid/custom", path, protocol),
            expected
        );
    }
    assert_eq!(
        transport::direct_url(
            "https://example.invalid/responses",
            "/responses",
            ValidationProtocol::OpenaiResponses
        ),
        "https://example.invalid/responses"
    );
    assert_eq!(
        transport::direct_url(
            "https://example.invalid/v1",
            "/v1/messages",
            ValidationProtocol::Anthropic
        ),
        "https://example.invalid/v1/messages"
    );
    assert_eq!(
        transport::direct_url(
            "https://example.invalid/v1beta",
            "/v1beta/models/test:streamGenerateContent?alt=sse",
            ValidationProtocol::Gemini
        ),
        "https://example.invalid/v1beta/models/test:streamGenerateContent?alt=sse"
    );
}

#[tokio::test]
async fn running_request_times_out_without_retry() {
    let p = ValidationProtocol::OpenaiChat;
    let mut slow = MockResponse::json(200, response(p, "CCS_OK"));
    slow.delay = Duration::from_secs(4);
    let server = MockServer::new(vec![slow]).await;
    let mut e = Executor::new(ValidationMode::Direct, None, Cancellation::default(), 2, 1).unwrap();
    let body = protocol::request(p, "test-model", protocol::CALL_PROMPT, 256, false);
    assert!(matches!(
        e.send(&pinned(p, &server.base), body).await,
        Err(RequestFailure::Timeout)
    ));
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn changed_response_model_and_comparison_evidence_are_redacted() {
    let p = ValidationProtocol::OpenaiChat;
    let mut value = response(p, "CCS_OK");
    value["model"] = json!(FAKE_KEY);
    let (result, _) = probe(Probe::Call, p, vec![MockResponse::json(200, value)]).await;
    let json = serde_json::to_string(&result).unwrap();
    assert!(!json.contains(FAKE_KEY));
    assert!(json.contains("redacted"));
}

#[tokio::test]
async fn all_protocols_report_thinking_and_cache_counter_evidence() {
    for p in [
        ValidationProtocol::OpenaiChat,
        ValidationProtocol::OpenaiResponses,
        ValidationProtocol::Anthropic,
        ValidationProtocol::Gemini,
    ] {
        let mut thinking = response(p, "42");
        match p {
            ValidationProtocol::OpenaiChat => {
                thinking["usage"]["completion_tokens_details"] = json!({"reasoning_tokens":4})
            }
            ValidationProtocol::OpenaiResponses => {
                thinking["usage"]["output_tokens_details"] = json!({"reasoning_tokens":4})
            }
            ValidationProtocol::Anthropic => thinking["content"].as_array_mut().unwrap().insert(
                0,
                json!({"type":"thinking","thinking":"synthetic private thought"}),
            ),
            ValidationProtocol::Gemini => {
                thinking["usageMetadata"]["thoughtsTokenCount"] = json!(4)
            }
        }
        let (result, _) = probe(Probe::Thinking, p, vec![MockResponse::json(200, thinking)]).await;
        assert_eq!(
            result.status,
            ProbeStatus::Passed,
            "{p:?}: {}",
            result.summary
        );
        assert!(!serde_json::to_string(&result)
            .unwrap()
            .contains("synthetic private thought"));
        let mut first = response(p, "CCS_OK");
        let mut second = response(p, "CCS_OK");
        match p {
            ValidationProtocol::OpenaiChat => {
                second["usage"]["prompt_tokens_details"] = json!({"cached_tokens":8000})
            }
            ValidationProtocol::OpenaiResponses => {
                second["usage"]["input_tokens_details"] = json!({"cached_tokens":8000})
            }
            ValidationProtocol::Anthropic => {
                first["usage"]["cache_creation_input_tokens"] = json!(8000);
                second["usage"]["cache_read_input_tokens"] = json!(8000);
            }
            ValidationProtocol::Gemini => {
                second["usageMetadata"]["cachedContentTokenCount"] = json!(8000)
            }
        }
        let (result, server) = probe(
            Probe::Cache,
            p,
            vec![
                MockResponse::json(200, first),
                MockResponse::json(200, second),
            ],
        )
        .await;
        assert_eq!(result.status, ProbeStatus::Passed, "{p:?}");
        assert_eq!(server.count(), 2);
    }
}

#[tokio::test]
async fn mid_run_config_changes_do_not_replace_the_pinned_key_or_endpoint() {
    let p = ValidationProtocol::OpenaiChat;
    let mut delayed = MockResponse::json(200, response(p, "CCS_OK"));
    delayed.delay = Duration::from_millis(150);
    let original = MockServer::new(vec![
        delayed,
        MockResponse::json(200, response(p, r#"{"answer":42,"label":"ccs"}"#)),
    ])
    .await;
    let replacement = MockServer::new(vec![]).await;
    let db = Arc::new(Database::memory().unwrap());
    let input = target_input(p);
    db.save_provider(&input.app_id, &provider(&input, &original.base, FAKE_KEY))
        .unwrap();
    let runtime = Arc::new(Runtime::default());
    let plan = runtime
        .prepare(
            &db,
            prepare_request(input.clone(), vec![Probe::Call, Probe::Structured]),
        )
        .unwrap();
    let run = runtime.start(db.clone(), None, &plan.id).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while original.count() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let changed = provider(&input, &replacement.base, "a-new-fake-key");
    db.save_provider(&input.app_id, &changed).unwrap();
    let done = completed(&runtime, &db, &run.id).await;
    assert_eq!(done.status, RunStatus::Completed);
    assert!(done.results.iter().all(|r| r.status == ProbeStatus::Passed));
    assert_eq!((original.count(), replacement.count()), (2, 0));
    for (_, headers, _) in original.state.requests.lock().unwrap().iter() {
        assert_eq!(headers["authorization"], format!("Bearer {FAKE_KEY}"));
    }
    assert_eq!(
        db.get_provider_by_id(&input.provider_id, &input.app_id)
            .unwrap()
            .unwrap()
            .settings_config,
        changed.settings_config
    );
}

#[tokio::test]
async fn history_failure_prevents_any_billable_request() {
    let p = ValidationProtocol::OpenaiChat;
    let server = MockServer::new(vec![]).await;
    let db = Arc::new(Database::memory().unwrap());
    let input = target_input(p);
    db.save_provider(&input.app_id, &provider(&input, &server.base, FAKE_KEY))
        .unwrap();
    db.list_model_validation_runs(None, None, 1).unwrap();
    db.conn.lock().unwrap().execute_batch("CREATE TRIGGER reject_validation_history BEFORE INSERT ON model_validation_runs BEGIN SELECT RAISE(ABORT, 'synthetic storage failure'); END;").unwrap();
    let runtime = Arc::new(Runtime::default());
    let plan = runtime
        .prepare(&db, prepare_request(input, vec![Probe::Call]))
        .unwrap();
    assert!(runtime.start(db, None, &plan.id).is_err());
    assert_eq!(server.count(), 0);
}

#[tokio::test]
async fn concurrency_is_bounded_and_queued_work_is_never_implicitly_started() {
    let p = ValidationProtocol::OpenaiChat;
    let delayed = || {
        let mut m = MockResponse::json(200, response(p, "CCS_OK"));
        m.delay = Duration::from_secs(10);
        m
    };
    let server = MockServer::new(vec![delayed(), delayed()]).await;
    let db = Arc::new(Database::memory().unwrap());
    let input = target_input(p);
    db.save_provider(&input.app_id, &provider(&input, &server.base, FAKE_KEY))
        .unwrap();
    let runtime = Arc::new(Runtime::default());
    let mut plans = Vec::new();
    for _ in 0..3 {
        plans.push(
            runtime
                .prepare(&db, prepare_request(input.clone(), vec![Probe::Call]))
                .unwrap(),
        );
    }
    let first = runtime.start(db.clone(), None, &plans[0].id).unwrap();
    let second = runtime.start(db.clone(), None, &plans[1].id).unwrap();
    assert!(runtime.start(db.clone(), None, &plans[2].id).is_err());
    runtime.cancel(&db, &first.id).unwrap();
    runtime.cancel(&db, &second.id).unwrap();
    assert_eq!(
        completed(&runtime, &db, &first.id).await.status,
        RunStatus::Cancelled
    );
    assert_eq!(
        completed(&runtime, &db, &second.id).await.status,
        RunStatus::Cancelled
    );
    assert!(server.count() <= 2);
}

#[test]
fn snapshot_fingerprints_are_stable_across_hashmap_reads_but_preserve_array_order() {
    let input = target_input(ValidationProtocol::OpenaiChat);
    let mut config = provider(&input, "https://example.invalid", FAKE_KEY);
    config.settings_config["ordered"] = json!([1, 2, {"b":2,"a":1}]);
    config.meta = Some(ProviderMeta {
        custom_endpoints: (0..8)
            .map(|n| {
                let url = format!("https://endpoint-{n}.invalid");
                (
                    url.clone(),
                    crate::settings::CustomEndpoint {
                        url,
                        added_at: n,
                        last_used: None,
                    },
                )
            })
            .collect(),
        ..Default::default()
    });
    let snapshot = |provider: Provider| {
        PinnedTarget::from_provider(AppType::OpenCode, provider, &input, ValidationMode::Direct)
            .unwrap()
            .fingerprint
    };
    let expected = snapshot(config.clone());
    let stored = serde_json::to_string(&config).unwrap();
    for _ in 0..64 {
        // Deserialization creates fresh independently seeded metadata HashMaps.
        assert_eq!(snapshot(serde_json::from_str(&stored).unwrap()), expected);
    }
    let mut reordered = config.clone();
    reordered.settings_config["ordered"] = json!([1, 2, {"a":1,"b":2}]);
    assert_eq!(snapshot(reordered), expected);
    config.settings_config["ordered"] = json!([2, 1, {"a":1,"b":2}]);
    assert_ne!(snapshot(config), expected);
}

#[tokio::test]
async fn all_probes_together_stay_within_the_preview_request_and_output_budgets() {
    let p = ValidationProtocol::Anthropic;
    let mut limited = response(p, "1 2 3 4 5");
    limited["stop_reason"] = json!("max_tokens");
    limited["usage"]["output_tokens"] = json!(16);
    let mut cache_first = response(p, "CCS_OK");
    cache_first["usage"]["cache_creation_input_tokens"] = json!(8000);
    let mut cache_second = response(p, "CCS_OK");
    cache_second["usage"]["cache_read_input_tokens"] = json!(8000);
    let mut primary_responses = vec![
        MockResponse::json(200, response(p, "CCS_OK")),
        MockResponse::sse("data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"CCS_OK\"}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n"),
        MockResponse::json(200, json!({"content":[{"type":"tool_use","id":"c1","name":"ccs_add","input":{"a":19,"b":23}}]})),
        MockResponse::json(200, response(p, "42")),
        MockResponse::json(200, response(p, r#"{"answer":42,"label":"ccs"}"#)),
        MockResponse::json(200, response(p, "image placeholder")),
        MockResponse::json(200, limited),
        MockResponse::json(200, cache_first),
        MockResponse::json(200, cache_second),
        MockResponse::json(200, signed_reply()),
        MockResponse::json(200, signed_reply()),
        MockResponse::json(200, response(p, "CCS_OK")),
        MockResponse::json(400, json!({"error":{"message":"Invalid signature in thinking block"}})),
        MockResponse::json(200, signed_reply()),
        MockResponse::json(200, response(p, "CCS_OK")),
    ];
    let mut secondary_responses = vec![
        MockResponse::json(200, signed_reply()),
        MockResponse::json(200, response(p, "CCS_OK")),
        MockResponse::json(200, response(p, "CCS_OK")),
    ];
    for _ in 0..5 {
        for answer in ["323", "-2,0,4,9", "amber"] {
            primary_responses.push(MockResponse::json(200, response(p, answer)));
            secondary_responses.push(MockResponse::json(200, response(p, answer)));
        }
    }
    let primary = MockServer::new(primary_responses).await;
    let secondary = MockServer::new(secondary_responses).await;
    let db = Arc::new(Database::memory().unwrap());
    let input = target_input(p);
    let mut other_input = input.clone();
    other_input.provider_id = "test-provider-b".into();
    db.save_provider(&input.app_id, &provider(&input, &primary.base, FAKE_KEY))
        .unwrap();
    db.save_provider(
        &other_input.app_id,
        &provider(&other_input, &secondary.base, "test-static-key-B"),
    )
    .unwrap();
    let runtime = Arc::new(Runtime::default());
    let probes = vec![
        Probe::Call,
        Probe::Stream,
        Probe::Tools,
        Probe::Structured,
        Probe::Image,
        Probe::OutputLimit,
        Probe::Cache,
        Probe::Thinking,
        Probe::Signature,
        Probe::CrossSignature,
        Probe::Comparison,
    ];
    let plan = runtime
        .prepare(
            &db,
            PrepareRequest {
                target: input,
                mode: ValidationMode::Direct,
                probes,
                comparison_target: Some(other_input),
                repeat_count: Some(5),
            },
        )
        .unwrap();
    assert_eq!((primary.count(), secondary.count()), (0, 0));
    assert_eq!(plan.max_requests, 48);
    assert_eq!(plan.max_output_tokens, 24_336);
    assert_eq!(plan.max_duration_seconds, 1800);
    let color = plan
        .id
        .bytes()
        .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32))
        % 3;
    primary.state.pending.lock().unwrap()[5] =
        MockResponse::json(200, response(p, ["RED", "GREEN", "BLUE"][color as usize]));
    let initial = runtime.start(db.clone(), None, &plan.id).unwrap();
    let run = completed(&runtime, &db, &initial.id).await;
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!((primary.count(), secondary.count()), (30, 18));
    assert_eq!(
        run.results.iter().map(|r| r.request_count).sum::<u32>(),
        plan.max_requests
    );
    for result in &run.results {
        assert_eq!(
            result.status,
            ProbeStatus::Passed,
            "{:?}: {}",
            result.probe,
            result.summary
        );
        assert_eq!(
            result.request_count,
            probes::budget(result.probe, 5, true).0
        );
    }
    let mut emitted_tokens = 0;
    for server in [&primary, &secondary] {
        for (_, _, body) in server.state.requests.lock().unwrap().iter() {
            emitted_tokens += output_token_limit(p, body);
        }
    }
    assert_eq!(emitted_tokens, u64::from(plan.max_output_tokens));
}

#[tokio::test]
async fn failing_selected_key_pool_member_never_uses_the_healthy_current_member() {
    use crate::provider_groups::{KeyPoolStrategy, ProviderGroup, ProviderGroupKind};
    let p = ValidationProtocol::OpenaiResponses;
    let server = MockServer::new(vec![
        MockResponse::json(401, json!({"error":{"message":"synthetic invalid key A"}})),
        MockResponse::json(200, response(p, "CCS_OK")),
    ])
    .await;
    let db = Arc::new(Database::memory().unwrap());
    let input = target_input(p);
    let group = ProviderGroup {
        id: "synthetic-key-pool".into(),
        app_type: input.app_id.clone(),
        name: "Test pool".into(),
        icon: None,
        icon_color: None,
        kind: ProviderGroupKind::Manual,
        normalized_base_url: None,
        sort_index: 0,
        collapsed: false,
        key_pool_enabled: true,
        key_pool_strategy: KeyPoolStrategy::RoundRobin,
        key_pool_max_retries: 3,
        key_pool_cooldown_ms: 1000,
        balance_template_id: None,
        created_at: 1,
        updated_at: 1,
    };
    db.create_provider_group(&group).unwrap();
    let mut other_input = input.clone();
    other_input.provider_id = "test-provider-b".into();
    for (input, key) in [(&input, FAKE_KEY), (&other_input, "test-static-key-B")] {
        let mut member = provider(input, &server.base, key);
        member.meta = Some(ProviderMeta {
            provider_group_id: Some(group.id.clone()),
            key_pool_enabled: Some(true),
            ..Default::default()
        });
        db.save_provider(&input.app_id, &member).unwrap();
    }
    db.set_current_provider(&input.app_id, &other_input.provider_id)
        .unwrap();
    let before_group = serde_json::to_value(db.get_provider_group(&group.id).unwrap()).unwrap();
    let before_members =
        serde_json::to_value(db.get_all_providers(&input.app_id).unwrap()).unwrap();
    let runtime = Arc::new(Runtime::default());
    let mut folder_input = input.clone();
    folder_input.provider_id = group.id.clone();
    assert!(runtime
        .prepare(&db, prepare_request(folder_input, vec![Probe::Call]))
        .is_err());
    let plan = runtime
        .prepare(&db, prepare_request(input.clone(), vec![Probe::Call]))
        .unwrap();
    let initial = runtime.start(db.clone(), None, &plan.id).unwrap();
    let run = completed(&runtime, &db, &initial.id).await;
    assert_eq!(run.results[0].status, ProbeStatus::Failed);
    assert_eq!(run.results[0].request_count, 1);
    let records = server.state.requests.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].1["authorization"], format!("Bearer {FAKE_KEY}"));
    assert_eq!(
        db.get_current_provider(&input.app_id).unwrap(),
        Some(other_input.provider_id)
    );
    assert_eq!(
        serde_json::to_value(db.get_provider_group(&group.id).unwrap()).unwrap(),
        before_group
    );
    assert_eq!(
        serde_json::to_value(db.get_all_providers(&input.app_id).unwrap()).unwrap(),
        before_members
    );
}

#[tokio::test]
async fn invalid_repeat_and_duplicate_probes_are_rejected_before_any_request() {
    let server = MockServer::new(vec![]).await;
    let db = Arc::new(Database::memory().unwrap());
    let input = target_input(ValidationProtocol::OpenaiChat);
    db.save_provider(&input.app_id, &provider(&input, &server.base, FAKE_KEY))
        .unwrap();
    let runtime = Arc::new(Runtime::default());
    for repeats in [0, 1, 6, u32::MAX] {
        let mut request = prepare_request(input.clone(), vec![Probe::Comparison]);
        request.repeat_count = Some(repeats);
        assert!(runtime.prepare(&db, request).is_err());
    }
    assert!(runtime
        .prepare(&db, prepare_request(input, vec![Probe::Call, Probe::Call]))
        .is_err());
    assert_eq!(server.count(), 0);
}

#[tokio::test]
async fn explicit_full_url_is_honored_without_appending_another_endpoint() {
    let p = ValidationProtocol::OpenaiResponses;
    let server = MockServer::new(vec![MockResponse::json(200, response(p, "CCS_OK"))]).await;
    let input = target_input(p);
    let mut config = provider(&input, &format!("{}/custom-action", server.base), FAKE_KEY);
    config.meta = Some(ProviderMeta {
        is_full_url: Some(true),
        ..Default::default()
    });
    let target =
        PinnedTarget::from_provider(AppType::Codex, config, &input, ValidationMode::Direct)
            .unwrap();
    let result = probes::run(Probe::Call, &mut executor(1), &target, None, 2, "fixture").await;
    assert_eq!(result.status, ProbeStatus::Passed);
    assert_eq!(server.state.requests.lock().unwrap()[0].0, "/custom-action");
}

#[test]
fn credential_labels_identify_key_replacement_without_exposing_either_key() {
    let input = target_input(ValidationProtocol::OpenaiChat);
    let label = |key: &str| {
        PinnedTarget::from_provider(
            AppType::OpenCode,
            provider(&input, "https://example.invalid", key),
            &input,
            ValidationMode::Direct,
        )
        .unwrap()
        .summary
        .credential_label
    };
    let a = label(FAKE_KEY);
    assert_eq!(a, label(FAKE_KEY));
    let b = label("synthetic-other-key");
    assert_ne!(a, b);
    assert!(!a.contains(FAKE_KEY));
    assert!(!b.contains("synthetic-other-key"));
}

#[test]
fn thinking_evidence_ignores_blank_content_but_includes_real_sse_deltas() {
    for (protocol, event, response) in [
        (
            ValidationProtocol::OpenaiChat,
            json!({"choices":[{"delta":{"reasoning_content":"thought"}}]}),
            json!({"choices":[{"message":{"reasoning_content":" \n "}}]}),
        ),
        (
            ValidationProtocol::OpenaiResponses,
            json!({"type":"response.reasoning_summary_text.delta","delta":"thought"}),
            json!({"output":[{"type":"reasoning","summary":[{"text":" \n "}],"encrypted_content":" "}]}),
        ),
        (
            ValidationProtocol::Anthropic,
            json!({"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"thought"}}),
            json!({"content":[{"type":"thinking","thinking":" \n ","signature":" "}]}),
        ),
        (
            ValidationProtocol::Gemini,
            json!({"candidates":[{"content":{"parts":[{"thought":true,"text":"thought"}]}}]}),
            json!({"candidates":[{"content":{"parts":[{"thought":true,"text":" \n ","thoughtSignature":" "}]}}]}),
        ),
    ] {
        let observed = protocol::observe(protocol, &response);
        assert!(!observed.thinking_present && !observed.signature_present);
        let mut decoder = SseDecoder::new(protocol);
        let blank = format!(
            "data: {}\n\n",
            event.to_string().replace(":\"thought\"", ":\" \"")
        );
        decoder.feed(blank.as_bytes(), 4).unwrap();
        assert_eq!(decoder.first_content_ms, None);
        assert!(!decoder.observation.thinking_present);
        decoder
            .feed(format!("data: {event}\n\n").as_bytes(), 25)
            .unwrap();
        assert_eq!(decoder.first_content_ms, Some(25));
        assert!(decoder.observation.thinking_present);
    }
}

#[tokio::test]
async fn chain_prepare_rejects_private_overrides_but_direct_does_not_apply_them() {
    let db = Arc::new(Database::memory().unwrap());
    let runtime = Arc::new(Runtime::default());
    let upstream = MockServer::new(vec![]).await;
    let input = target_input(ValidationProtocol::OpenaiResponses);
    let mut target = provider(&input, &upstream.base, FAKE_KEY);
    target
        .meta
        .get_or_insert_with(ProviderMeta::default)
        .local_proxy_request_overrides = Some(crate::provider::LocalProxyRequestOverrides {
        body: Some(json!({"instructions":"private-fixture-content"})),
        headers: Default::default(),
    });
    db.save_provider(&input.app_id, &target).unwrap();
    let mut request = prepare_request(input, vec![Probe::Call]);
    request.mode = ValidationMode::Ccs;
    let error = runtime
        .prepare(&db, request.clone())
        .unwrap_err()
        .to_string();
    assert!(error.contains("非合成内容"));
    assert!(!error.contains("private-fixture-content"));
    request.mode = ValidationMode::Direct;
    assert!(runtime.prepare(&db, request).is_ok());
    assert!(upstream.state.requests.lock().unwrap().is_empty());
}

#[test]
fn chain_target_must_match_the_actual_adapter_key_and_endpoint() {
    let input = target_input(ValidationProtocol::Gemini);
    let mut target = provider(&input, "https://example.invalid", FAKE_KEY);
    // The generic direct resolver supports GOOGLE_API_KEY, while the current
    // Gemini forwarder only consumes GEMINI_API_KEY or the direct apiKey field.
    target.settings_config["env"]
        .as_object_mut()
        .unwrap()
        .remove("GEMINI_API_KEY");
    target.settings_config["env"]["GOOGLE_API_KEY"] = json!(FAKE_KEY);
    target.settings_config["apiKey"] = json!("different-static-fixture-key");
    assert!(PinnedTarget::from_provider(
        AppType::Gemini,
        target.clone(),
        &input,
        ValidationMode::Direct
    )
    .is_ok());
    let error = PinnedTarget::from_provider(AppType::Gemini, target, &input, ValidationMode::Ccs)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("凭据与检测目标不一致"));
    assert!(!error.contains(FAKE_KEY) && !error.contains("different-static-fixture-key"));

    let input = target_input(ValidationProtocol::OpenaiResponses);
    let mut target = provider(&input, "https://chosen.invalid/v1", FAKE_KEY);
    target
        .settings_config
        .as_object_mut()
        .unwrap()
        .remove("base_url");
    target.settings_config["config"] = json!(
        "model_provider = 'chosen'\n[model_providers.old]\nbase_url = 'https://old.invalid/v1'\n[model_providers.chosen]\nbase_url = 'https://chosen.invalid/v1'\nwire_api = 'responses'"
    );
    let error = PinnedTarget::from_provider(AppType::Codex, target, &input, ValidationMode::Ccs)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("端点与检测目标不一致"));
}

#[test]
fn chain_preflight_rejects_dynamic_grok_auth_before_an_adapter_can_expand_it() {
    let input = target_input(ValidationProtocol::OpenaiResponses);
    let mut target = provider(&input, "https://example.invalid/v1", FAKE_KEY);
    target
        .settings_config
        .as_object_mut()
        .unwrap()
        .remove("auth");
    target.settings_config["config"] = json!(format!(
        "experimental_bearer_token = '{FAKE_KEY}'\n[models]\ndefault = 'proxy'\n[model.proxy]\nname = 'Synthetic'\nmodel = 'test-model'\nbase_url = 'https://example.invalid/v1'\nenv_key = 'CCS_VALIDATION_MUST_NOT_EXPAND'\napi_backend = 'responses'\ncontext_window = 131072"
    ));
    let error = PinnedTarget::from_provider(AppType::Codex, target, &input, ValidationMode::Ccs)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("不展开环境变量凭据"));
    assert!(!error.contains(FAKE_KEY) && !error.contains("CCS_VALIDATION_MUST_NOT_EXPAND"));
}
