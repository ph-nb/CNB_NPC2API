use crate::{
    api,
    config::{Auth, Config, Features, Server, Upstream},
    fc,
    model::Surface,
};
use axum::{
    body::{to_bytes, Body},
    extract::{Path, State},
    http::{header, HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures_util::stream;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::Notify;
use tower::ServiceExt;

#[derive(Default)]
struct Mock {
    calls: Mutex<Vec<(String, HeaderMap, Value)>>,
    csrf_failures: AtomicUsize,
    malformed: AtomicUsize,
    slow_home: AtomicBool,
    require_session: AtomicBool,
    fetch_started: Notify,
    release_fetch: Notify,
    release_stream: Notify,
}

fn consume_failure(counter: &AtomicUsize) -> bool {
    let mut remaining = counter.load(Ordering::SeqCst);
    while remaining > 0 {
        match counter.compare_exchange_weak(
            remaining,
            remaining - 1,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => return true,
            Err(actual) => remaining = actual,
        }
    }
    false
}

async fn homepage(State(mock): State<Arc<Mock>>, headers: HeaderMap) -> Response {
    if mock.slow_home.load(Ordering::SeqCst) {
        mock.fetch_started.notify_one();
        mock.release_fetch.notified().await;
    }
    if mock.require_session.load(Ordering::SeqCst)
        && !headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| s.contains("session_id=local-session"))
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    (
        [(
            header::SET_COOKIE,
            "csrfkey=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; Path=/; HttpOnly",
        )],
        "<script>window.csrftoken=\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"</script>",
    )
        .into_response()
}
async fn openai(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    fake(mock, headers, body, "openai").await
}
async fn openai_responses_native(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    mock.calls
        .lock()
        .unwrap()
        .push(("responses_native".into(), headers, body.clone()));
    let response = json!({
        "id":crate::model::id("resp_native_"),
        "object":"response",
        "status":"completed",
        "model":body["model"],
        "store":false,
        "output":[
            {"type":"web_search_call","id":"search_1","status":"completed"},
            {"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Native result","annotations":[]}]}
        ]
    });
    if body["stream"].as_bool().unwrap_or(false) {
        if body["input"].to_string().contains("TEST_NATIVE_EOF") {
            return fragmented(sse(
                &json!({"type":"response.created","response":{"id":response["id"],"status":"in_progress"}}),
                Some("response.created"),
            ));
        }
        return fragmented(
            sse(
                &json!({"type":"response.created","response":{"id":response["id"],"status":"in_progress"}}),
                Some("response.created"),
            ) + &sse(
                &json!({"type":"response.completed","response":response}),
                Some("response.completed"),
            ) + "data: [DONE]\n\n",
        );
    }
    Json(response).into_response()
}
async fn cnb(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    fake(mock, headers, body, "cnb").await
}
async fn anthropic(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    fake(mock, headers, body, "anthropic").await
}
async fn gemini(
    State(mock): State<Arc<Mock>>,
    Path(path): Path<String>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Response {
    body["stream"] = json!(path.ends_with(":streamGenerateContent"));
    fake(mock, headers, body, "gemini").await
}
fn sse(value: &Value, event: Option<&str>) -> String {
    format!(
        "{}data: {}\n\n",
        event.map(|e| format!("event: {e}\n")).unwrap_or_default(),
        value
    )
}
fn fragmented(data: String) -> Response {
    let chunks = data
        .into_bytes()
        .chunks(7)
        .map(|b| Ok::<Bytes, Infallible>(Bytes::copy_from_slice(b)))
        .collect::<Vec<_>>();
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(stream::iter(chunks)),
    )
        .into_response()
}
async fn fake(mock: Arc<Mock>, headers: HeaderMap, body: Value, kind: &str) -> Response {
    mock.calls
        .lock()
        .unwrap()
        .push((kind.into(), headers.clone(), body.clone()));
    if kind == "cnb" {
        if body.to_string().contains("TEST_TIMEOUT") {
            mock.release_stream.notified().await;
        }
        let cookies = headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if headers.get("csrftoken").and_then(|v| v.to_str().ok())
            != Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            || !cookies.contains("csrfkey=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        {
            return (StatusCode::UNAUTHORIZED, "csrf").into_response();
        }
        if mock.require_session.load(Ordering::SeqCst)
            && !cookies.contains("session_id=local-session")
        {
            return (StatusCode::UNAUTHORIZED, "not logged in").into_response();
        }
        if consume_failure(&mock.csrf_failures) {
            return (StatusCode::UNAUTHORIZED, "csrf rejected").into_response();
        }
        if body.to_string().contains("TEST_NOT_LOGIN") {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"errcode":401,"errmsg":"[NOT_LOGIN]User not logged in"})),
            )
                .into_response();
        }
    }
    if body.to_string().contains("TEST_UPSTREAM_429") {
        return (StatusCode::TOO_MANY_REQUESTS, "secret upstream details").into_response();
    }
    if body.to_string().contains("TEST_EOF") {
        return fragmented(sse(
            &json!({"choices":[{"delta":{"content":"partial"},"finish_reason":null}]}),
            None,
        ));
    }
    if body.to_string().contains("TEST_DELAY") {
        let content = sse(
            &json!({"choices":[{"delta":{"content":"first"},"finish_reason":null}]}),
            None,
        );
        let end = sse(
            &json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
            None,
        ) + "data: [DONE]\n\n";
        let output = async_stream::stream! { yield Ok::<Bytes,Infallible>(Bytes::from(content)); mock.release_stream.notified().await; yield Ok(Bytes::from(end)); };
        return (
            [(header::CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(output),
        )
            .into_response();
    }
    let prompt = body.to_string().contains("TOOL CALL PROTOCOL");
    let native = body["tools"].as_array().is_some_and(|v| !v.is_empty());
    let tool_name = if body.to_string().contains("Action name: tool_0") {
        "tool_0"
    } else {
        "get_weather"
    };
    let text = if prompt && consume_failure(&mock.malformed) {
        "<|XYML|tool_calls>".into()
    } else if prompt {
        fc::render(tool_name, &json!({"city":"Tokyo"}), "XYML")
    } else {
        "Hello world".into()
    };
    let is_stream = kind == "cnb" || body["stream"].as_bool().unwrap_or(false);
    if !is_stream {
        return Json(match kind {
            "anthropic" => json!({"id":"up","content":if native {json!([{"type":"tool_use","id":"call_1","name":"get_weather","input":{"city":"Tokyo"}}])} else {json!([{"type":"text","text":text}])},"stop_reason":if native {"tool_use"} else {"end_turn"},"usage":{"input_tokens":3,"output_tokens":2}}),
            "gemini" => json!({"candidates":[{"content":{"parts":if native {json!([{"functionCall":{"name":"get_weather","args":{"city":"Tokyo"}}}])} else {json!([{"text":text}])}},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2,"totalTokenCount":5}}),
            _ => json!({"id":"up","choices":[{"message":if native {json!({"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Tokyo\"}"}}]})} else {json!({"role":"assistant","content":text})},"finish_reason":if native {"tool_calls"} else {"stop"}}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
        }).into_response();
    }
    let data = match kind {
        "anthropic" => {
            let mut s = sse(
                &json!({"type":"message_start","message":{"usage":{"input_tokens":3}}}),
                Some("message_start"),
            );
            if native {
                s += &sse(
                    &json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"get_weather","input":{}}}),
                    Some("content_block_start"),
                );
                s += &sse(
                    &json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}),
                    Some("content_block_delta"),
                );
                s += &sse(
                    &json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"Tokyo\"}"}}),
                    Some("content_block_delta"),
                );
            } else {
                s += &sse(
                    &json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
                    Some("content_block_delta"),
                );
            }
            s += &sse(
                &json!({"type":"message_delta","delta":{"stop_reason":if native {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":2}}),
                Some("message_delta"),
            );
            s += &sse(&json!({"type":"message_stop"}), Some("message_stop"));
            s
        }
        "gemini" => sse(
            &json!({"candidates":[{"content":{"parts":if native {json!([{"functionCall":{"name":"get_weather","args":{"city":"Tokyo"}}}])} else {json!([{"text":text}])}},"finishReason":"STOP"}]}),
            None,
        ),
        _ => {
            let mut s = String::new();
            if native {
                s += &sse(
                    &json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_weather","arguments":"{\"city\":"}}]},"finish_reason":null}]}),
                    None,
                );
                s += &sse(
                    &json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Tokyo\"}"}}]},"finish_reason":null}]}),
                    None,
                );
            } else {
                s += &sse(
                    &json!({"id":"up","model":"test-model","choices":[{"delta":{"content":text},"finish_reason":null}]}),
                    None,
                );
            }
            s += &sse(
                &json!({"choices":[{"delta":{},"finish_reason":if native {"tool_calls"} else {"stop"}}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
                None,
            );
            s += "data: [DONE]\n\n";
            s
        }
    };
    fragmented(data)
}

async fn mock_server() -> (String, Arc<Mock>, tokio::task::JoinHandle<()>) {
    let mock = Arc::new(Mock::default());
    let router = Router::new()
        .route("/", get(homepage))
        .route("/ai/chat/completions", post(cnb))
        .route("/v1/chat/completions", post(openai))
        .route("/v1/responses", post(openai_responses_native))
        .route("/v1/messages", post(anthropic))
        .route("/v1beta/models/{model}", post(gemini))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}"), mock, task)
}
fn config(base: &str, kind: &str, native: bool) -> Config {
    let up = Upstream {
        name: "mock".into(),
        kind: kind.into(),
        base_url: if kind == "openai_compat" {
            format!("{base}/v1")
        } else {
            base.into()
        },
        api_key: "upstream-test-key".into(),
        models: vec!["test-model".into()],
        native_fc: native,
        is_default: true,
        pool_min: 1,
        pool_max: 2,
        ..Default::default()
    };
    Config {
        server: Server::default(),
        client_authentication: Auth {
            enabled: true,
            allowed_keys: vec!["client-test-key".into()],
        },
        upstreams: vec![up],
        features: Features::default(),
        ..Default::default()
    }
}
async fn app(base: &str, kind: &str, native: bool) -> Router {
    let cfg = config(base, kind, native);
    cfg.validate().unwrap();
    api::router(api::App::new(cfg).await.unwrap())
}
fn client_request(surface: Surface, is_stream: bool) -> (&'static str, Value) {
    let schema =
        json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]});
    match surface {
        Surface::Chat => (
            "/v1/chat/completions",
            json!({"model":"test-model","stream":is_stream,"messages":[{"role":"user","content":"Weather?"}],"tools":[{"type":"function","function":{"name":"get_weather","parameters":schema}}]}),
        ),
        Surface::Anthropic => (
            "/v1/messages",
            json!({"model":"test-model","stream":is_stream,"max_tokens":100,"messages":[{"role":"user","content":"Weather?"}],"tools":[{"name":"get_weather","input_schema":schema}]}),
        ),
        Surface::Responses => (
            "/v1/responses",
            json!({"model":"test-model","stream":is_stream,"input":"Weather?","tools":[{"type":"function","name":"get_weather","parameters":schema}]}),
        ),
        Surface::Gemini => (
            if is_stream {
                "/v1beta/models/test-model:streamGenerateContent"
            } else {
                "/v1beta/models/test-model:generateContent"
            },
            json!({"contents":[{"role":"user","parts":[{"text":"Weather?"}]}],"tools":[{"functionDeclarations":[{"name":"get_weather","parameters":schema}]}]}),
        ),
    }
}
async fn call(
    router: Router,
    path: &str,
    body: Value,
    key: Option<&str>,
) -> (StatusCode, HeaderMap, String) {
    let mut builder = Request::post(path).header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = key {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    let response = router
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn protocol_and_upstream_matrix() {
    let (base, mock, server) = mock_server().await;
    let surfaces = [
        Surface::Chat,
        Surface::Anthropic,
        Surface::Gemini,
        Surface::Responses,
    ];
    let mut count = 0;
    for kind in ["cnb", "openai_compat", "anthropic", "gemini"] {
        for native in [false, true] {
            if kind == "cnb" && native {
                continue;
            }
            let router = app(&base, kind, native).await;
            for surface in surfaces {
                for is_stream in [false, true] {
                    let (path, body) = client_request(surface, is_stream);
                    let (status, headers, body) =
                        call(router.clone(), path, body, Some("client-test-key")).await;
                    assert_eq!(
                        status,
                        StatusCode::OK,
                        "{kind} {native} {surface:?} {is_stream}: {body}"
                    );
                    assert_eq!(
                        headers["x-toolforge-fc-mode"],
                        if native { "native" } else { "prompt" }
                    );
                    if is_stream {
                        assert!(
                            body.contains("get_weather") && body.contains("Tokyo"),
                            "{kind} {surface:?}: {body}"
                        );
                        let end = match surface {
                            Surface::Chat => "data: [DONE]",
                            Surface::Anthropic => "event: message_stop",
                            Surface::Responses => "event: response.completed",
                            Surface::Gemini => "finishReason",
                        };
                        assert!(body.contains(end), "{kind} {surface:?}: {body}");
                        if surface == Surface::Responses {
                            let events: Vec<Value> = body
                                .lines()
                                .filter_map(|s| s.strip_prefix("data: "))
                                .filter_map(|s| serde_json::from_str(s).ok())
                                .collect();
                            let done = events
                                .iter()
                                .find(|v| v["type"] == "response.output_item.done")
                                .unwrap();
                            let completed = events
                                .iter()
                                .find(|v| v["type"] == "response.completed")
                                .unwrap();
                            assert_eq!(
                                done["item"]["id"],
                                completed["response"]["output"][0]["id"]
                            );
                        }
                    } else {
                        let v: Value = serde_json::from_str(&body).unwrap();
                        match surface {
                            Surface::Chat => {
                                assert_eq!(
                                    v["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
                                    "get_weather"
                                );
                                assert!(v["choices"][0]["message"]["tool_calls"][0]["function"]
                                    ["arguments"]
                                    .as_str()
                                    .unwrap()
                                    .contains("Tokyo"));
                            }
                            Surface::Anthropic => {
                                assert_eq!(v["content"][0]["name"], "get_weather");
                                assert_eq!(v["content"][0]["input"]["city"], "Tokyo");
                            }
                            Surface::Gemini => {
                                assert_eq!(
                                    v["candidates"][0]["content"]["parts"][0]["functionCall"]
                                        ["name"],
                                    "get_weather"
                                );
                                assert_eq!(
                                    v["candidates"][0]["content"]["parts"][0]["functionCall"]
                                        ["args"]["city"],
                                    "Tokyo"
                                );
                            }
                            Surface::Responses => {
                                assert_eq!(v["output"][0]["name"], "get_weather");
                                assert!(v["output"][0]["arguments"]
                                    .as_str()
                                    .unwrap()
                                    .contains("Tokyo"));
                            }
                        }
                    }
                    let calls = mock.calls.lock().unwrap();
                    let (_, headers, upstream) = calls.last().unwrap();
                    if kind == "cnb" {
                        assert!(upstream.get("tools").is_none());
                        assert_eq!(
                            headers["csrftoken"],
                            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                        );
                    }
                    if native {
                        assert!(upstream["tools"].is_array());
                    } else {
                        assert!(upstream.get("tools").is_none());
                        assert!(upstream.to_string().contains("TOOL CALL PROTOCOL"));
                    }
                    count += 1;
                }
            }
        }
    }
    assert_eq!(count, 56);
    server.abort();
}

#[tokio::test]
async fn authentication_csrf_retry_and_error_redaction() {
    let (base, mock, server) = mock_server().await;
    let cnb = app(&base, "cnb", false).await;
    let (path, body) = client_request(Surface::Chat, false);
    assert_eq!(
        call(cnb.clone(), path, body.clone(), None).await.0,
        StatusCode::UNAUTHORIZED
    );
    mock.csrf_failures.store(1, Ordering::SeqCst);
    let before = mock.calls.lock().unwrap().len();
    assert_eq!(
        call(cnb.clone(), path, body, Some("client-test-key"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(mock.calls.lock().unwrap().len() - before, 2);
    let pool_response = cnb
        .clone()
        .oneshot(
            Request::get("/pool")
                .header(header::AUTHORIZATION, "Bearer client-test-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let pool_body = to_bytes(pool_response.into_body(), 10000).await.unwrap();
    let pool_text = String::from_utf8(pool_body.to_vec()).unwrap();
    assert!(!pool_text.contains("aaaaaaaa") && !pool_text.contains("bbbbbbbb"));
    let (status, _, out) = call(
        cnb,
        "/v1/chat/completions",
        json!({"model":"test-model","messages":[{"role":"user","content":"TEST_NOT_LOGIN"}]}),
        Some("client-test-key"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(out.contains("CNB_SESSION_COOKIE"));
    let openai = app(&base, "openai_compat", true).await;
    let (status, _, body) = call(
        openai,
        "/v1/chat/completions",
        json!({"model":"test-model","messages":[{"role":"user","content":"TEST_UPSTREAM_429"}]}),
        Some("client-test-key"),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(!body.contains("secret upstream details"));
    server.abort();
}

#[tokio::test]
async fn plain_text_matrix_and_auth_headers() {
    let (base, mock, server) = mock_server().await;
    let mut count = 0;
    for kind in ["cnb", "openai_compat", "anthropic", "gemini"] {
        let router = app(&base, kind, false).await;
        for surface in [
            Surface::Chat,
            Surface::Anthropic,
            Surface::Gemini,
            Surface::Responses,
        ] {
            for is_stream in [false, true] {
                let (path, mut body) = client_request(surface, is_stream);
                body.as_object_mut().unwrap().remove("tools");
                let (status, headers, out) =
                    call(router.clone(), path, body, Some("client-test-key")).await;
                assert_eq!(status, StatusCode::OK, "{kind} {surface:?}: {out}");
                assert_eq!(headers["x-toolforge-fc-mode"], "passthrough");
                assert!(out.contains("Hello world"), "{kind} {surface:?}: {out}");
                assert!(!mock
                    .calls
                    .lock()
                    .unwrap()
                    .last()
                    .unwrap()
                    .2
                    .to_string()
                    .contains("TOOL CALL PROTOCOL"));
                count += 1;
            }
        }
        for name in ["x-api-key", "x-goog-api-key"] {
            let response = router
                .clone()
                .oneshot(
                    Request::get("/v1/models")
                        .header(name, "client-test-key")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
    }
    assert_eq!(count, 32);
    server.abort();
}

#[tokio::test]
async fn retry_obfuscation_and_incomplete_stream() {
    let (base, mock, server) = mock_server().await;
    let mut cfg = config(&base, "openai_compat", false);
    cfg.features.obfuscate_tool_names = true;
    let router = api::router(api::App::new(cfg).await.unwrap());
    mock.malformed.store(1, Ordering::SeqCst);
    let (path, body) = client_request(Surface::Chat, false);
    let (status, _, out) = call(router.clone(), path, body, Some("client-test-key")).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "get_weather"
    );
    assert_eq!(mock.calls.lock().unwrap().len(), 2);
    let (status,_,out)=call(router.clone(),"/v1/chat/completions",json!({"model":"test-model","stream":true,"messages":[{"role":"user","content":"TEST_EOF"}]}),Some("client-test-key")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(out.contains("event: error"));
    assert!(!out.contains("[DONE]"));
    mock.malformed.store(1, Ordering::SeqCst);
    let (path, body) = client_request(Surface::Chat, true);
    let (status, _, out) = call(router.clone(), path, body, Some("client-test-key")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(out.contains("event: error"));
    assert!(!out.contains("[DONE]"));
    mock.malformed.store(3, Ordering::SeqCst);
    let (path, body) = client_request(Surface::Chat, false);
    assert_eq!(
        call(router.clone(), path, body, Some("client-test-key"))
            .await
            .0,
        StatusCode::BAD_GATEWAY
    );
    let cnb = app(&base, "cnb", false).await;
    let (status, _, out) = call(
        cnb,
        "/v1/chat/completions",
        json!({"model":"test-model","messages":[{"role":"user","content":"TEST_EOF"}]}),
        Some("client-test-key"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{out}");
    server.abort();
}

#[tokio::test]
async fn multimodal_input_is_forwarded_or_rejected_explicitly() {
    let (base, mock, server) = mock_server().await;
    let body = json!({"model":"test-model","messages":[{"role":"user","content":[{"type":"text","text":"describe"},{"type":"image_url","image_url":{"url":"https://example.invalid/image.png"}}]}]});
    let cnb = app(&base, "cnb", false).await;
    assert_eq!(
        call(
            cnb,
            "/v1/chat/completions",
            body.clone(),
            Some("client-test-key")
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let openai = app(&base, "openai_compat", true).await;
    assert_eq!(
        call(
            openai,
            "/v1/chat/completions",
            body,
            Some("client-test-key")
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        mock.calls.lock().unwrap().last().unwrap().2["messages"][0]["content"][1]["type"],
        "image_url"
    );
    server.abort();
}

#[tokio::test]
async fn image_inputs_convert_between_client_and_upstream_protocols() {
    let (base, mock, server) = mock_server().await;
    let data_url = "data:image/png;base64,aGVsbG8=";
    let requests = [
        (
            "/v1/chat/completions",
            json!({"model":"test-model","messages":[{"role":"user","content":[{"type":"text","text":"describe"},{"type":"image_url","image_url":{"url":data_url}}]}]}),
        ),
        (
            "/v1/responses",
            json!({"model":"test-model","input":[{"role":"user","content":[{"type":"input_text","text":"describe"},{"type":"input_image","image_url":data_url}]}]}),
        ),
        (
            "/v1/messages",
            json!({"model":"test-model","max_tokens":100,"messages":[{"role":"user","content":[{"type":"text","text":"describe"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}]}]}),
        ),
        (
            "/v1beta/models/test-model:generateContent",
            json!({"contents":[{"role":"user","parts":[{"text":"describe"},{"inlineData":{"mimeType":"image/png","data":"aGVsbG8="}}]}]}),
        ),
    ];
    for kind in ["openai_compat", "anthropic", "gemini"] {
        let router = app(&base, kind, true).await;
        for (path, body) in &requests {
            let (status, _, output) =
                call(router.clone(), path, body.clone(), Some("client-test-key")).await;
            assert_eq!(status, StatusCode::OK, "{kind} {path}: {output}");
            let calls = mock.calls.lock().unwrap();
            let request = &calls.last().unwrap().2;
            match kind {
                "openai_compat" => assert_eq!(
                    request["messages"][0]["content"][1]["image_url"]["url"],
                    data_url
                ),
                "anthropic" => assert_eq!(
                    request["messages"][0]["content"][1]["source"]["data"],
                    "aGVsbG8="
                ),
                "gemini" => assert_eq!(
                    request["contents"][0]["parts"][1]["inlineData"]["data"],
                    "aGVsbG8="
                ),
                _ => unreachable!(),
            }
        }
    }
    let router = app(&base, "anthropic", false).await;
    let mut prompt_request = requests[0].1.clone();
    prompt_request["tools"] = json!([{"type":"function","function":{"name":"get_weather","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}]);
    assert_eq!(
        call(
            router,
            requests[0].0,
            prompt_request,
            Some("client-test-key")
        )
        .await
        .0,
        StatusCode::OK
    );
    let calls = mock.calls.lock().unwrap();
    let user = calls.last().unwrap().2["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .unwrap();
    assert_eq!(user["content"][1]["source"]["data"], "aGVsbG8=");
    drop(calls);
    server.abort();
}

#[tokio::test]
async fn unsupported_images_and_files_are_not_silently_dropped() {
    let (base, mock, server) = mock_server().await;
    let mut cfg = config(&base, "openai_compat", true);
    cfg.upstreams[0].models = vec!["openai-model".into()];
    let mut cnb = config(&base, "cnb", false).upstreams.remove(0);
    cnb.name = "cnb".into();
    cnb.models = vec!["cnb-model".into()];
    cfg.upstreams.push(cnb);
    let router = api::router(api::App::new(cfg).await.unwrap());
    let (status, _, first) = call(router.clone(), "/v1/responses", json!({"model":"openai-model","input":[{"role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,aGVsbG8="}]}]}), Some("client-test-key")).await;
    assert_eq!(status, StatusCode::OK);
    let first: Value = serde_json::from_str(&first).unwrap();
    let (status, _, _) = call(
        router.clone(),
        "/v1/responses",
        json!({"model":"cnb-model","previous_response_id":first["id"],"input":"describe"}),
        Some("client-test-key"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = call(router.clone(), "/v1/responses", json!({"model":"openai-model","input":[{"role":"user","content":[{"type":"input_file","file_id":"file_test"}]}]}), Some("client-test-key")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = call(router, "/v1beta/models/openai-model:generateContent", json!({"contents":[{"role":"user","parts":[{"fileData":{"mimeType":"image/png","fileUri":"gs://private/image.png"}}]}]}), Some("client-test-key")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(mock.calls.lock().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn unsupported_request_features_are_rejected_before_forwarding() {
    let (base, mock, server) = mock_server().await;
    let router = app(&base, "openai_compat", true).await;
    let cases = [
        (
            "/v1/chat/completions",
            json!({"model":"test-model","n":2,"messages":[{"role":"user","content":"hi"}]}),
        ),
        (
            "/v1/chat/completions",
            json!({"model":"test-model","tool_choice":"required","messages":[{"role":"user","content":"hi"}]}),
        ),
        (
            "/v1/responses",
            json!({"model":"test-model","input":"hi","background":true}),
        ),
        (
            "/v1/responses",
            json!({"model":"test-model","input":"hi","tools":[{"type":"function"}]}),
        ),
        (
            "/v1beta/models/test-model:generateContent",
            json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}],"tools":[{}]}),
        ),
    ];
    for (path, body) in cases {
        assert_eq!(
            call(router.clone(), path, body, Some("client-test-key"))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert!(mock.calls.lock().unwrap().is_empty());
    server.abort();
}

#[tokio::test]
async fn native_responses_tools_and_history_are_scoped_and_replayed() {
    let (base, mock, server) = mock_server().await;
    let mut cfg = config(&base, "openai_compat", true);
    cfg.client_authentication
        .allowed_keys
        .push("other-key".into());
    let router = api::router(api::App::new(cfg).await.unwrap());
    let first_request = json!({"model":"test-model","input":"find a source","instructions":"old instruction","tools":[{"type":"web_search"}]});
    let (status, _, first) = call(
        router.clone(),
        "/v1/responses",
        first_request,
        Some("client-test-key"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let first: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(first["store"], true);
    let id = first["id"].as_str().unwrap();
    assert_eq!(
        state_request(router.clone(), "GET", id, "other-key")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        state_request(router.clone(), "GET", id, "client-test-key")
            .await
            .1,
        first
    );
    let (status, _, second) = call(router.clone(), "/v1/responses", json!({"model":"test-model","previous_response_id":id,"input":"summarize it","store":false}), Some("client-test-key")).await;
    assert_eq!(status, StatusCode::OK);
    let second: Value = serde_json::from_str(&second).unwrap();
    assert_eq!(second["previous_response_id"], id);
    assert_eq!(second["store"], false);
    assert_eq!(
        state_request(
            router.clone(),
            "GET",
            second["id"].as_str().unwrap(),
            "client-test-key"
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    {
        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls[0].0, "responses_native");
        assert_eq!(calls[0].2["store"], false);
        assert!(calls[0].2["include"]
            .as_array()
            .unwrap()
            .contains(&json!("reasoning.encrypted_content")));
        assert_eq!(calls[1].2["input"][0]["content"], "find a source");
        assert_eq!(calls[1].2["input"][1]["type"], "web_search_call");
        assert_eq!(calls[1].2["input"][2]["type"], "message");
        assert_eq!(calls[1].2["input"][3]["content"], "summarize it");
        assert!(calls[1].2.get("previous_response_id").is_none());
        assert!(calls[1].2.get("instructions").is_none());
    }
    assert_eq!(
        state_request(router, "DELETE", id, "client-test-key")
            .await
            .0,
        StatusCode::OK
    );
    server.abort();
}

#[tokio::test]
async fn native_responses_stream_and_canonical_history_are_preserved() {
    let (base, mock, server) = mock_server().await;
    let router = app(&base, "openai_compat", true).await;
    let (status, _, first) = call(
        router.clone(),
        "/v1/responses",
        json!({"model":"test-model","input":"first turn"}),
        Some("client-test-key"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let first: Value = serde_json::from_str(&first).unwrap();
    let (status, _, streamed) = call(router.clone(), "/v1/responses", json!({"model":"test-model","previous_response_id":first["id"],"input":"search next","tools":[{"type":"web_search"}],"stream":true}), Some("client-test-key")).await;
    assert_eq!(status, StatusCode::OK);
    let events: Vec<Value> = streamed
        .lines()
        .filter_map(|s| s.strip_prefix("data: "))
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect();
    let created = events
        .iter()
        .find(|event| event["type"] == "response.created")
        .unwrap();
    let completed = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .unwrap();
    let id = completed["response"]["id"].as_str().unwrap();
    assert_eq!(created["response"]["id"], id);
    assert_eq!(
        state_request(router.clone(), "GET", id, "client-test-key")
            .await
            .1,
        completed["response"]
    );
    {
        let calls = mock.calls.lock().unwrap();
        let input = &calls[1].2["input"];
        assert_eq!(input[0]["content"], "first turn");
        assert_eq!(input[1]["content"], "Hello world");
        assert_eq!(input[2]["content"], "search next");
    }
    assert_eq!(
        call(
            router.clone(),
            "/v1/responses",
            json!({"model":"test-model","previous_response_id":id,"input":"follow up"}),
            Some("client-test-key")
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, _, interrupted) = call(router.clone(), "/v1/responses", json!({"model":"test-model","input":"TEST_NATIVE_EOF","tools":[{"type":"web_search"}],"stream":true}), Some("client-test-key")).await;
    let events: Vec<Value> = interrupted
        .lines()
        .filter_map(|s| s.strip_prefix("data: "))
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect();
    let created = events
        .iter()
        .find(|event| event["type"] == "response.created")
        .unwrap();
    assert!(events.iter().any(|event| event["type"] == "error"));
    assert_eq!(
        state_request(
            router,
            "GET",
            created["response"]["id"].as_str().unwrap(),
            "client-test-key"
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    server.abort();
}

#[tokio::test]
async fn native_non_function_tools_require_a_matching_upstream() {
    let (base, mock, server) = mock_server().await;
    let scenarios = [
        (
            "anthropic",
            "/v1/messages",
            json!({"model":"test-model","max_tokens":100,"messages":[{"role":"user","content":"search"}],"tools":[{"type":"web_search_20250305","name":"web_search"}]}),
        ),
        (
            "gemini",
            "/v1beta/models/test-model:generateContent",
            json!({"contents":[{"role":"user","parts":[{"text":"search"}]}],"tools":[{"googleSearch":{}}]}),
        ),
        (
            "openai_compat",
            "/v1/chat/completions",
            json!({"model":"test-model","messages":[{"role":"user","content":"run"}],"tools":[{"type":"custom","custom":{"name":"example"}}]}),
        ),
    ];
    for (kind, path, body) in scenarios {
        let router = app(&base, kind, true).await;
        assert_eq!(
            call(router, path, body.clone(), Some("client-test-key"))
                .await
                .0,
            StatusCode::OK
        );
        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls.last().unwrap().2["tools"], body["tools"]);
    }
    let before = mock.calls.lock().unwrap().len();
    let cnb = app(&base, "cnb", false).await;
    assert_eq!(
        call(
            cnb,
            "/v1/responses",
            json!({"model":"test-model","input":"search","tools":[{"type":"web_search"}]}),
            Some("client-test-key")
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(mock.calls.lock().unwrap().len(), before);
    server.abort();
}

async fn state_request(router: Router, method: &str, id: &str, key: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(format!("/v1/responses/{id}"))
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn responses_state_is_scoped_retrievable_and_deletable() {
    let (base, mock, server) = mock_server().await;
    let mut cfg = config(&base, "openai_compat", true);
    cfg.client_authentication
        .allowed_keys
        .push("other-key".into());
    let router = api::router(api::App::new(cfg).await.unwrap());
    let (status, _, first) = call(
        router.clone(),
        "/v1/responses",
        json!({"model":"test-model","instructions":"old instruction","input":"first question"}),
        Some("client-test-key"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let first: Value = serde_json::from_str(&first).unwrap();
    let id = first["id"].as_str().unwrap();
    assert_eq!(first["store"], true);
    assert_eq!(
        state_request(router.clone(), "GET", id, "client-test-key")
            .await
            .1,
        first
    );
    assert_eq!(
        state_request(router.clone(), "GET", id, "other-key")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let (status, _, second) = call(router.clone(), "/v1/responses", json!({"model":"test-model","previous_response_id":id,"instructions":"new instruction","input":"second question","store":false}), Some("client-test-key")).await;
    assert_eq!(status, StatusCode::OK);
    let second: Value = serde_json::from_str(&second).unwrap();
    assert_eq!(second["previous_response_id"], id);
    assert_eq!(second["store"], false);
    assert_eq!(
        state_request(
            router.clone(),
            "GET",
            second["id"].as_str().unwrap(),
            "client-test-key"
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    {
        let calls = mock.calls.lock().unwrap();
        let messages = calls[1].2["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["content"], "new instruction");
        assert_eq!(messages[1]["content"], "first question");
        assert_eq!(messages[2]["content"], "Hello world");
        assert_eq!(messages[3]["content"], "second question");
    }
    assert_eq!(
        state_request(router.clone(), "DELETE", id, "other-key")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        state_request(router.clone(), "DELETE", id, "client-test-key")
            .await
            .0,
        StatusCode::OK
    );
    let (status, _, _) = call(
        router,
        "/v1/responses",
        json!({"model":"test-model","previous_response_id":id,"input":"third question"}),
        Some("client-test-key"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    server.abort();
}

#[tokio::test]
async fn responses_tool_results_replay_the_original_call() {
    let (base, mock, server) = mock_server().await;
    let router = app(&base, "openai_compat", true).await;
    let (path, request) = client_request(Surface::Responses, false);
    let (status, _, first) = call(router.clone(), path, request, Some("client-test-key")).await;
    assert_eq!(status, StatusCode::OK);
    let first: Value = serde_json::from_str(&first).unwrap();
    let id = first["id"].as_str().unwrap();
    let call_id = first["output"][0]["call_id"].as_str().unwrap();
    let (status, _, _) = call(router, "/v1/responses", json!({"model":"test-model","previous_response_id":id,"input":[{"type":"function_call_output","call_id":call_id,"output":"sunny"}]}), Some("client-test-key")).await;
    assert_eq!(status, StatusCode::OK);
    let calls = mock.calls.lock().unwrap();
    let messages = calls[1].2["messages"].as_array().unwrap();
    assert_eq!(messages[1]["tool_calls"][0]["id"], call_id);
    assert_eq!(
        messages[1]["tool_calls"][0]["function"]["name"],
        "get_weather"
    );
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["tool_call_id"], call_id);
    assert_eq!(messages[2]["content"], "sunny");
    server.abort();
}

#[tokio::test]
async fn streamed_responses_are_stored_only_after_completion() {
    let (base, mock, server) = mock_server().await;
    let router = app(&base, "openai_compat", true).await;
    let (status, _, body) = call(
        router.clone(),
        "/v1/responses",
        json!({"model":"test-model","input":"stream first","stream":true}),
        Some("client-test-key"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events: Vec<Value> = body
        .lines()
        .filter_map(|s| s.strip_prefix("data: "))
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect();
    let completed = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .unwrap();
    let id = completed["response"]["id"].as_str().unwrap();
    assert_eq!(
        state_request(router.clone(), "GET", id, "client-test-key")
            .await
            .1,
        completed["response"]
    );
    assert_eq!(
        call(
            router.clone(),
            "/v1/responses",
            json!({"model":"test-model","previous_response_id":id,"input":"stream followup"}),
            Some("client-test-key")
        )
        .await
        .0,
        StatusCode::OK
    );
    {
        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls[1].2["messages"][0]["content"], "stream first");
        assert_eq!(calls[1].2["messages"][1]["content"], "Hello world");
    }
    let (_, _, failed) = call(
        router.clone(),
        "/v1/responses",
        json!({"model":"test-model","input":"TEST_EOF","stream":true}),
        Some("client-test-key"),
    )
    .await;
    let events: Vec<Value> = failed
        .lines()
        .filter_map(|s| s.strip_prefix("data: "))
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect();
    let created = events
        .iter()
        .find(|event| event["type"] == "response.created")
        .unwrap();
    assert!(events.iter().any(|event| event["type"] == "error"));
    assert_eq!(
        state_request(
            router,
            "GET",
            created["response"]["id"].as_str().unwrap(),
            "client-test-key"
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    server.abort();
}

#[tokio::test]
async fn native_text_stream_is_incremental() {
    let (base, mock, server) = mock_server().await;
    let router = app(&base, "openai_compat", true).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let gateway = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let response=reqwest::Client::new().post(format!("http://{addr}/v1/chat/completions")).bearer_auth("client-test-key")
        .json(&json!({"model":"test-model","stream":true,"messages":[{"role":"user","content":"TEST_DELAY"}]})).send().await.unwrap();
    let mut events = response.bytes_stream().eventsource();
    let first = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(Ok(event)) = events.next().await {
            if event.data.contains("first") {
                return true;
            }
        }
        false
    })
    .await
    .unwrap();
    assert!(first);
    mock.release_stream.notify_one();
    let complete = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(Ok(event)) = events.next().await {
            if event.data == "[DONE]" {
                return true;
            }
        }
        false
    })
    .await
    .unwrap();
    assert!(complete);
    gateway.abort();
    server.abort();
}

#[tokio::test]
async fn reload_keeps_existing_streams_and_rejects_invalid_changes() {
    let (base, mock, server) = mock_server().await;
    let cfg = config(&base, "openai_compat", true);
    let (router, handle) = api::reloadable_router(api::App::new(cfg.clone()).await.unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let gateway = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::new();
    let response = client.post(format!("http://{addr}/v1/chat/completions")).bearer_auth("client-test-key")
        .json(&json!({"model":"test-model","stream":true,"messages":[{"role":"user","content":"TEST_DELAY"}]})).send().await.unwrap();
    let mut events = response.bytes_stream().eventsource();
    assert!(tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(Ok(event)) = events.next().await {
            if event.data.contains("first") {
                return true;
            }
        }
        false
    })
    .await
    .unwrap());
    let mut changed = cfg.clone();
    changed.client_authentication.allowed_keys = vec!["new-key".into()];
    handle.replace(changed.clone()).await.unwrap();
    assert_eq!(
        client
            .get(format!("http://{addr}/v1/models"))
            .bearer_auth("client-test-key")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(format!("http://{addr}/v1/models"))
            .bearer_auth("new-key")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let mut invalid = changed;
    invalid.server.port += 1;
    assert!(handle.replace(invalid).await.is_err());
    assert_eq!(
        client
            .get(format!("http://{addr}/v1/models"))
            .bearer_auth("new-key")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    mock.release_stream.notify_one();
    assert!(tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(Ok(event)) = events.next().await {
            if event.data == "[DONE]" {
                return true;
            }
        }
        false
    })
    .await
    .unwrap());
    gateway.abort();
    server.abort();
}

#[tokio::test]
async fn reload_watches_config_and_keeps_last_valid_version() {
    let (base, _, server) = mock_server().await;
    let path = std::env::temp_dir().join(format!("cnb-forge-reload-{}.yaml", uuid::Uuid::new_v4()));
    let raw = format!("client_authentication:\n  enabled: true\n  allowed_keys: [old-key]\nupstreams:\n  - name: mock\n    type: openai_compat\n    base_url: {base}/v1\n    models: [test-model]\n");
    std::fs::write(&path, &raw).unwrap();
    let (router, handle) =
        api::reloadable_router(api::App::new(Config::parse(&raw).unwrap()).await.unwrap());
    let watch = tokio::spawn(crate::reload::watch(
        handle,
        path.clone(),
        raw.clone(),
        None,
        None,
    ));
    std::fs::write(&path, raw.replace("old-key", "new-key")).unwrap();
    let status = |router: Router, key: &'static str| async move {
        router
            .oneshot(
                Request::get("/v1/models")
                    .header(header::AUTHORIZATION, format!("Bearer {key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    };
    assert!(tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            if status(router.clone(), "new-key").await == StatusCode::OK {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap());
    std::fs::write(&path, "server: [").unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(status(router.clone(), "new-key").await, StatusCode::OK);
    assert_eq!(status(router, "old-key").await, StatusCode::UNAUTHORIZED);
    watch.abort();
    std::fs::remove_file(path).unwrap();
    server.abort();
}

#[tokio::test]
async fn cancelled_pool_expansion_releases_reservation() {
    let (base, mock, server) = mock_server().await;
    let pool = crate::pool::Pool::new(
        &base,
        1,
        2,
        Duration::from_secs(60),
        Duration::from_secs(180),
        "",
    )
    .await
    .unwrap();
    let first = pool.acquire().await.unwrap();
    mock.slow_home.store(true, Ordering::SeqCst);
    let started = mock.fetch_started.notified();
    let cloned = pool.clone();
    let task = tokio::spawn(async move { cloned.acquire().await });
    tokio::time::timeout(Duration::from_secs(3), started)
        .await
        .unwrap();
    task.abort();
    let _ = task.await;
    mock.slow_home.store(false, Ordering::SeqCst);
    mock.release_fetch.notify_waiters();
    let second = tokio::time::timeout(Duration::from_secs(3), pool.acquire())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pool.stats().len(), 2);
    drop(first);
    drop(second);
    server.abort();
}

#[tokio::test]
async fn cnb_requests_honor_configured_timeout() {
    let (base, mock, server) = mock_server().await;
    let mut cfg = config(&base, "cnb", false);
    cfg.server.timeout_seconds = 1;
    let router = api::router(api::App::new(cfg).await.unwrap());
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        call(
            router,
            "/v1/chat/completions",
            json!({"model":"test-model","messages":[{"role":"user","content":"TEST_TIMEOUT"}]}),
            Some("client-test-key"),
        ),
    )
    .await;
    mock.release_stream.notify_waiters();
    server.abort();
    let (status, _, _) = result.expect("CNB request did not respect its configured timeout");
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn authenticated_cnb_session_is_paired_with_csrf_and_redacted() {
    let (base, mock, server) = mock_server().await;
    mock.require_session.store(true, Ordering::SeqCst);
    let anonymous = config(&base, "cnb", false);
    assert!(api::App::new(anonymous).await.is_err());
    let mut cfg = config(&base, "cnb", false);
    cfg.upstreams[0].session_cookie = "session_id=local-session".into();
    cfg.validate().unwrap();
    let router = api::router(api::App::new(cfg).await.unwrap());
    let (path, body) = client_request(Surface::Chat, false);
    assert_eq!(
        call(router.clone(), path, body, Some("client-test-key"))
            .await
            .0,
        StatusCode::OK
    );
    {
        let captured = mock.calls.lock().unwrap();
        let (_, headers, _) = captured.last().unwrap();
        let cookies = headers[header::COOKIE].to_str().unwrap();
        assert!(cookies.contains("session_id=local-session"));
        assert!(cookies.contains("csrfkey="));
    }
    let response = router
        .oneshot(
            Request::get("/pool")
                .header(header::AUTHORIZATION, "Bearer client-test-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = to_bytes(response.into_body(), 10000).await.unwrap();
    assert!(!String::from_utf8(bytes.to_vec())
        .unwrap()
        .contains("local-session"));
    let mut invalid = config("http://example.com", "cnb", false);
    invalid.upstreams[0].session_cookie = "session_id=value".into();
    assert!(invalid.validate().is_err());
    server.abort();
}
