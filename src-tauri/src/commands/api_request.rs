//! Explicit, one-shot API debugging. No shell, persistence, retries or failover.

use futures::future::{AbortHandle, Abortable};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{ipc::Channel, State};

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Default)]
pub struct ApiRequestState {
    active: Mutex<HashMap<String, AbortHandle>>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiRequest {
    url: String,
    headers: BTreeMap<String, String>,
    body: String,
    timeout_secs: u64,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ApiRequestEvent {
    Started,
    Headers {
        status: u16,
        headers: Vec<(String, String)>,
    },
    Chunk {
        data: Vec<u8>,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
    duration_ms: u64,
    complete: bool,
    error: Option<String>,
}

struct ActiveRequest<'a> {
    state: &'a ApiRequestState,
    id: String,
}

impl Drop for ActiveRequest<'_> {
    fn drop(&mut self) {
        if let Ok(mut active) = self.state.active.lock() {
            active.remove(&self.id);
        }
    }
}

impl ApiRequestState {
    fn cancel(&self, request_id: &str) -> Result<(), String> {
        let active = self.active.lock().map_err(|_| "Request lock unavailable")?;
        if let Some(handle) = active.get(request_id) {
            handle.abort();
        }
        Ok(())
    }
}

#[tauri::command]
pub async fn execute_api_request(
    state: State<'_, ApiRequestState>,
    request_id: String,
    request: ApiRequest,
    on_data: Channel<ApiRequestEvent>,
) -> Result<ApiResponse, String> {
    run_request(&state, request_id, request, |event| {
        on_data
            .send(event)
            .map_err(|_| "apiRequest.receiverClosed".to_string())
    })
    .await
}

#[tauri::command]
pub fn cancel_api_request(
    state: State<'_, ApiRequestState>,
    request_id: String,
) -> Result<(), String> {
    state.cancel(&request_id)
}

async fn run_request(
    state: &ApiRequestState,
    request_id: String,
    request: ApiRequest,
    on_event: impl Fn(ApiRequestEvent) -> Result<(), String>,
) -> Result<ApiResponse, String> {
    uuid::Uuid::parse_str(&request_id).map_err(|_| "Invalid request ID")?;
    let (handle, registration) = AbortHandle::new_pair();
    {
        let mut active = state
            .active
            .lock()
            .map_err(|_| "Request lock unavailable")?;
        if active.contains_key(&request_id) || active.len() >= 4 {
            return Err("apiRequest.alreadyRunning".to_string());
        }
        active.insert(request_id.clone(), handle);
    }
    let _active = ActiveRequest {
        state,
        id: request_id,
    };
    // Acknowledgement makes the cancel button safe even if IPC calls are scheduled out of order.
    on_event(ApiRequestEvent::Started)?;
    Abortable::new(
        send_request(request, on_event, MAX_RESPONSE_BYTES),
        registration,
    )
    .await
    .map_err(|_| "apiRequest.cancelled".to_string())?
}

fn validate_request(request: &ApiRequest) -> Result<(url::Url, HeaderMap), String> {
    let url = url::Url::parse(&request.url).map_err(|_| "apiRequest.invalidUrl")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("apiRequest.invalidUrl".to_string());
    }
    if !(1..=600).contains(&request.timeout_secs) {
        return Err("apiRequest.invalidTimeout".to_string());
    }
    if request.body.len() > MAX_REQUEST_BYTES {
        return Err("apiRequest.requestTooLarge".to_string());
    }
    let body: serde_json::Value =
        serde_json::from_str(&request.body).map_err(|_| "apiRequest.invalidBody")?;
    if !body.is_object() || request.headers.len() > 64 {
        return Err("apiRequest.invalidBody".to_string());
    }
    let mut headers = HeaderMap::new();
    for (name, value) in &request.headers {
        if name.len() > 256 || value.len() > 16 * 1024 {
            return Err("apiRequest.invalidHeaders".to_string());
        }
        let name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| "apiRequest.invalidHeaders")?;
        if matches!(
            name.as_str(),
            "host" | "content-length" | "transfer-encoding" | "connection" | "upgrade"
        ) {
            return Err("apiRequest.invalidHeaders".to_string());
        }
        let value = HeaderValue::from_str(value).map_err(|_| "apiRequest.invalidHeaders")?;
        headers.insert(name, value);
    }
    Ok((url, headers))
}

fn network_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "apiRequest.timedOut".to_string()
    } else {
        // A provider URL can contain query credentials. Never include it in an error log/UI.
        format!("Request failed: {}", error.without_url())
    }
}

async fn send_request(
    request: ApiRequest,
    on_event: impl Fn(ApiRequestEvent) -> Result<(), String>,
    response_limit: usize,
) -> Result<ApiResponse, String> {
    let (url, headers) = validate_request(&request)?;
    let client =
        crate::proxy::http_client::create_with_redirect_policy(reqwest::redirect::Policy::none())?;
    let start = Instant::now();
    let mut response = client
        .post(url)
        .headers(headers)
        .body(request.body)
        .timeout(Duration::from_secs(request.timeout_secs))
        .send()
        .await
        .map_err(network_error)?;
    // Keep HTTP failures and duplicate response headers (e.g. Set-Cookie) intact.
    let status = response.status().as_u16();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                value.as_bytes().iter().map(|&b| char::from(b)).collect(),
            )
        })
        .collect();
    on_event(ApiRequestEvent::Headers {
        status,
        headers: headers.clone(),
    })?;
    let mut bytes = Vec::new();
    let mut error = None;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if chunk.len() > response_limit.saturating_sub(bytes.len()) {
                    error = Some("apiRequest.responseTooLarge".to_string());
                    break;
                }
                bytes.extend_from_slice(&chunk);
                on_event(ApiRequestEvent::Chunk {
                    data: chunk.to_vec(),
                })?;
            }
            Ok(None) => break,
            Err(e) => {
                error = Some(network_error(e));
                break;
            }
        }
    }
    let body = match String::from_utf8(bytes) {
        Ok(body) => body,
        Err(e) => {
            error.get_or_insert_with(|| "apiRequest.nonUtf8Response".to_string());
            String::from_utf8_lossy(e.as_bytes()).into_owned()
        }
    };
    Ok(ApiResponse {
        status,
        headers,
        body,
        duration_ms: start.elapsed().as_millis() as u64,
        complete: error.is_none(),
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::StatusCode, response::IntoResponse, routing::post, Router};
    use bytes::Bytes;
    use futures::StreamExt;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct TestServer {
        url: String,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for TestServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    impl TestServer {
        async fn new(router: Router) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            Self { url, task }
        }
        fn request(&self, path: &str) -> ApiRequest {
            ApiRequest { url: format!("{}{path}", self.url), headers: BTreeMap::from([
                ("authorization".to_string(), "Bearer test-secret".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ]), body: r#"{"model":"test","messages":[{"role":"user","content":"今日 AI"}],"stream":true}"#.to_string(), timeout_secs: 5 }
        }
    }

    #[tokio::test]
    async fn keeps_full_error_body_and_duplicate_headers() {
        let server = TestServer::new(Router::new().route(
            "/error",
            post(|headers: HeaderMap, body: String| async move {
                assert_eq!(headers["authorization"], "Bearer test-secret");
                assert!(body.contains("今日 AI"));
                let mut response = (
                    StatusCode::UNAUTHORIZED,
                    r#"{"error":{"message":"wrong API key","details":[1,2,3]}}"#,
                )
                    .into_response();
                response
                    .headers_mut()
                    .append("set-cookie", HeaderValue::from_static("a=1"));
                response
                    .headers_mut()
                    .append("set-cookie", HeaderValue::from_static("b=2"));
                response
            }),
        ))
        .await;
        let response = send_request(server.request("/error"), |_| Ok(()), 4096)
            .await
            .unwrap();
        assert_eq!(response.status, 401);
        assert_eq!(
            response.body,
            r#"{"error":{"message":"wrong API key","details":[1,2,3]}}"#
        );
        assert_eq!(
            response
                .headers
                .iter()
                .filter(|(name, _)| name == "set-cookie")
                .count(),
            2
        );
        assert!(response.complete);
    }

    #[tokio::test]
    async fn preserves_every_sse_byte_including_unicode_and_done() {
        let raw = "event: delta\ndata: {\"text\":\"你好 AI\"}\n\ndata: [DONE]\n\n";
        let server = TestServer::new(Router::new().route(
            "/sse",
            post(move || async move {
                let chunks = raw
                    .as_bytes()
                    .chunks(2)
                    .map(|part| Ok::<_, std::io::Error>(Bytes::copy_from_slice(part)))
                    .collect::<Vec<_>>();
                (
                    [("content-type", "text/event-stream")],
                    Body::from_stream(futures::stream::iter(chunks)),
                )
            }),
        ))
        .await;
        let received = Mutex::new(Vec::new());
        let response = send_request(
            server.request("/sse"),
            |event| {
                if let ApiRequestEvent::Chunk { data } = event {
                    received.lock().unwrap().extend(data);
                }
                Ok(())
            },
            4096,
        )
        .await
        .unwrap();
        assert_eq!(response.body, raw);
        assert_eq!(*received.lock().unwrap(), raw.as_bytes());
        assert!(response.complete);
    }

    #[tokio::test]
    async fn does_not_follow_redirects_or_forward_provider_credentials() {
        let hits = Arc::new(AtomicUsize::new(0));
        let captured = hits.clone();
        let server = TestServer::new(
            Router::new()
                .route(
                    "/redirect",
                    post(|| async {
                        (
                            StatusCode::TEMPORARY_REDIRECT,
                            [("location", "/target")],
                            "redirect body",
                        )
                    }),
                )
                .route(
                    "/target",
                    post(move || {
                        captured.fetch_add(1, Ordering::SeqCst);
                        async { "unexpected" }
                    }),
                ),
        )
        .await;
        let response = send_request(server.request("/redirect"), |_| Ok(()), 4096)
            .await
            .unwrap();
        assert_eq!(response.status, 307);
        assert_eq!(response.body, "redirect body");
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn timeout_keeps_partial_response_and_reports_incomplete() {
        let server = TestServer::new(Router::new().route(
            "/slow",
            post(|| async {
                let chunks = futures::stream::once(async {
                    Ok::<_, std::io::Error>(Bytes::from_static(b"data: first\n\n"))
                })
                .chain(futures::stream::pending());
                Body::from_stream(chunks).into_response()
            }),
        ))
        .await;
        let mut request = server.request("/slow");
        request.timeout_secs = 1;
        let response = send_request(request, |_| Ok(()), 4096).await.unwrap();
        assert!(!response.complete);
        assert_eq!(response.body, "data: first\n\n");
        assert_eq!(response.error.as_deref(), Some("apiRequest.timedOut"));
    }

    #[tokio::test]
    async fn response_limit_is_explicit_instead_of_silent_truncation() {
        let server =
            TestServer::new(Router::new().route("/large", post(|| async { "a".repeat(2048) })))
                .await;
        let response = send_request(server.request("/large"), |_| Ok(()), 32)
            .await
            .unwrap();
        assert!(!response.complete);
        assert_eq!(
            response.error.as_deref(),
            Some("apiRequest.responseTooLarge")
        );
    }

    #[tokio::test]
    async fn cancellation_releases_only_the_matching_request() {
        let server = TestServer::new(Router::new().route(
            "/slow",
            post(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "late"
            }),
        ))
        .await;
        let state = Arc::new(ApiRequestState::default());
        let task_state = state.clone();
        let id = uuid::Uuid::new_v4().to_string();
        let task_id = id.clone();
        let request = server.request("/slow");
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let ready_tx = Mutex::new(Some(ready_tx));
        let task = tokio::spawn(async move {
            run_request(&task_state, task_id, request, |event| {
                if matches!(event, ApiRequestEvent::Started) {
                    ready_tx.lock().unwrap().take().unwrap().send(()).unwrap();
                }
                Ok(())
            })
            .await
        });
        ready_rx.await.unwrap();
        state.cancel("another-id").unwrap();
        assert_eq!(state.active.lock().unwrap().len(), 1);
        state.cancel(&id).unwrap();
        assert_eq!(
            task.await.unwrap().err().as_deref(),
            Some("apiRequest.cancelled")
        );
        assert!(state.active.lock().unwrap().is_empty());
    }

    #[test]
    fn rejects_unsafe_urls_headers_and_oversized_requests() {
        let mut request = ApiRequest {
            url: "file:///private".into(),
            headers: BTreeMap::new(),
            body: "{}".into(),
            timeout_secs: 120,
        };
        assert!(validate_request(&request).is_err());
        request.url = "https://example.com/v1/responses".into();
        request.headers.insert(
            "authorization".into(),
            "Bearer secret\r\nHost: other".into(),
        );
        assert!(validate_request(&request).is_err());
        request.headers.clear();
        request
            .headers
            .insert("Host".into(), "other.example".into());
        assert!(validate_request(&request).is_err());
        request.headers.clear();
        request.body = "x".repeat(MAX_REQUEST_BYTES + 1);
        assert!(validate_request(&request).is_err());
    }
}
