use crate::{
    config::Config,
    fc,
    model::{self, Message, Request, Surface},
    response_store::{History, PendingNativeResponse, PendingResponse, ResponseStore},
    streaming,
    upstream::{Transport, UpstreamStatus},
    wire::{self, Mode},
};
use axum::{
    extract::{DefaultBodyLimit, Path, Request as AxumRequest, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock,
    },
};
use tower::ServiceExt;

#[derive(Default)]
struct Metrics {
    requests: AtomicU64,
    failures: Arc<AtomicU64>,
    retries: AtomicU64,
}
#[derive(Clone)]
pub struct App {
    cfg: Arc<Config>,
    transport: Arc<Transport>,
    metrics: Arc<Metrics>,
    response_store: Arc<ResponseStore>,
}
impl App {
    pub async fn new(cfg: Config) -> anyhow::Result<Self> {
        let transport = Transport::new(&cfg.upstreams, cfg.server.timeout_seconds).await?;
        Ok(Self {
            cfg: Arc::new(cfg),
            transport: Arc::new(transport),
            metrics: Arc::new(Metrics::default()),
            response_store: ResponseStore::new(),
        })
    }
    async fn reconfigured(&self, cfg: Config) -> anyhow::Result<Self> {
        let transport = Transport::new(&cfg.upstreams, cfg.server.timeout_seconds).await?;
        Ok(Self {
            cfg: Arc::new(cfg),
            transport: Arc::new(transport),
            metrics: self.metrics.clone(),
            response_store: self.response_store.clone(),
        })
    }
}

struct ActiveApp {
    app: App,
    router: Router,
}

#[derive(Clone)]
pub struct ReloadHandle {
    active: Arc<RwLock<ActiveApp>>,
    gate: Arc<tokio::sync::Mutex<()>>,
}

impl ReloadHandle {
    pub async fn replace(&self, cfg: Config) -> anyhow::Result<()> {
        let _guard = self.gate.lock().await;
        cfg.validate()?;
        let previous = self.active.read().unwrap().app.clone();
        if cfg.server.host != previous.cfg.server.host
            || cfg.server.port != previous.cfg.server.port
        {
            anyhow::bail!("changing the listener requires a restart");
        }
        let app = previous.reconfigured(cfg).await?;
        let router = router(app.clone());
        *self.active.write().unwrap() = ActiveApp { app, router };
        Ok(())
    }
}

pub fn reloadable_router(app: App) -> (Router, ReloadHandle) {
    let handle = ReloadHandle {
        active: Arc::new(RwLock::new(ActiveApp {
            router: router(app.clone()),
            app,
        })),
        gate: Arc::new(tokio::sync::Mutex::new(())),
    };
    (
        Router::new()
            .fallback(any(dispatch))
            .with_state(handle.clone()),
        handle,
    )
}

async fn dispatch(State(handle): State<ReloadHandle>, request: AxumRequest) -> Response {
    let router = handle.active.read().unwrap().router.clone();
    match router.oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}
pub fn router(app: App) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/pool", get(pool))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .route("/chat/completions", post(chat))
        .route("/v1/messages", post(anthropic))
        .route("/messages", post(anthropic))
        .route("/anthropic/v1/messages", post(anthropic))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/anthropic/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/responses", post(responses))
        .route("/responses", post(responses))
        .route(
            "/v1/responses/{id}",
            get(get_response).delete(delete_response),
        )
        .route("/responses/{id}", get(get_response).delete(delete_response))
        .route("/v1beta/models/{model_path}", post(gemini))
        .route("/v1/models/{model_path}", post(gemini))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(app)
}
fn key(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            let mut parts = s.trim().splitn(2, char::is_whitespace);
            let first = parts.next().unwrap_or("");
            if first.eq_ignore_ascii_case("bearer") {
                parts.next().unwrap_or("").trim()
            } else {
                s.trim()
            }
        })
        .filter(|s| !s.is_empty())
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            headers
                .get("x-goog-api-key")
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.is_empty())
        })
}
fn authorized(app: &App, headers: &HeaderMap) -> bool {
    !app.cfg.client_authentication.enabled
        || key(headers).is_some_and(|key| {
            app.cfg
                .client_authentication
                .allowed_keys
                .iter()
                .any(|allowed| allowed == key)
        })
}
fn error(code: StatusCode, message: &str) -> Response {
    (code,Json(json!({"error":{"message":message,"type":if code == StatusCode::UNAUTHORIZED {"authentication_error"} else {"api_error"}}}))).into_response()
}
fn unauthorized() -> Response {
    error(StatusCode::UNAUTHORIZED, "invalid api key")
}
async fn health(State(app): State<App>) -> Json<Value> {
    let count: usize = app.transport.pools.values().map(|p| p.stats().len()).sum();
    Json(
        json!({"status":"ok","version":crate::VERSION,"upstreams":app.cfg.upstreams.iter().map(|u|&u.name).collect::<Vec<_>>(),"poolSize":count}),
    )
}
async fn metrics(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }
    if !app.cfg.features.enable_metrics {
        return Json(json!({"enabled":false})).into_response();
    }
    Json(json!({"requests":app.metrics.requests.load(Ordering::Relaxed),"failures":app.metrics.failures.load(Ordering::Relaxed),"retries":app.metrics.retries.load(Ordering::Relaxed)})).into_response()
}
async fn pool(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }
    let pools: HashMap<_, _> = app
        .transport
        .pools
        .iter()
        .map(|(name, p)| (name, p.stats()))
        .collect();
    Json(json!({"pools":pools})).into_response()
}
async fn models(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }
    let mut seen = HashSet::new();
    let mut data = Vec::new();
    for up in &app.cfg.upstreams {
        for m in &up.models {
            if m != "*" && seen.insert(m.clone()) {
                data.push(json!({"id":m,"object":"model","owned_by":up.name,"permission":[]}));
            }
        }
    }
    for alias in app.cfg.routing.aliases.keys() {
        if seen.insert(alias.clone()) {
            data.push(json!({"id":alias,"object":"model","owned_by":"routing","permission":[]}));
        }
    }
    Json(json!({"object":"list","data":data})).into_response()
}
async fn chat(State(app): State<App>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    handle(app, headers, body, Surface::Chat, None, None).await
}
async fn anthropic(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    handle(app, headers, body, Surface::Anthropic, None, None).await
}
async fn responses(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    handle(app, headers, body, Surface::Responses, None, None).await
}
async fn get_response(
    State(app): State<App>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }
    match app.response_store.get(&id, key(&headers).unwrap_or("")) {
        Some((response, _)) => Json(response).into_response(),
        None => error(StatusCode::NOT_FOUND, "response not found"),
    }
}
async fn delete_response(
    State(app): State<App>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }
    if app.response_store.delete(&id, key(&headers).unwrap_or("")) {
        Json(json!({"id":id,"object":"response.deleted","deleted":true})).into_response()
    } else {
        error(StatusCode::NOT_FOUND, "response not found")
    }
}
async fn gemini(
    State(app): State<App>,
    Path(path): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some((model, method)) = path.rsplit_once(':') else {
        return error(StatusCode::NOT_FOUND, "expected model:method");
    };
    let stream = match method {
        "generateContent" => false,
        "streamGenerateContent" => true,
        _ => return error(StatusCode::NOT_FOUND, "unknown model method"),
    };
    handle(
        app,
        headers,
        body,
        Surface::Gemini,
        Some(model.to_owned()),
        Some(stream),
    )
    .await
}
async fn count_tokens(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }
    let mut chars = model::as_text(&body["system"]).chars().count();
    for m in body["messages"].as_array().into_iter().flatten() {
        chars += model::as_text(&m["content"]).chars().count();
    }
    for t in body["tools"].as_array().into_iter().flatten() {
        chars += t.to_string().chars().count();
    }
    Json(json!({"input_tokens":chars.div_ceil(4)})).into_response()
}
fn with_mode(mut response: Response, mode: Mode) -> Response {
    response
        .headers_mut()
        .insert("x-toolforge-fc-mode", HeaderValue::from_static(mode.name()));
    response
}
fn has_native_tool(body: &Value, surface: Surface) -> bool {
    body["tools"].as_array().is_some_and(|items| {
        items.iter().any(|tool| match surface {
            Surface::Chat | Surface::Responses => {
                tool["type"].as_str().is_some_and(|kind| kind != "function")
            }
            Surface::Anthropic => {
                tool["type"].as_str().is_some_and(|kind| kind != "custom")
                    || tool["name"].as_str().is_none()
            }
            Surface::Gemini => tool.as_object().is_none_or(|fields| {
                fields
                    .keys()
                    .any(|name| name != "functionDeclarations" && name != "function_declarations")
            }),
        })
    })
}

fn validate_tools(body: &Value, surface: Surface) -> Result<(), &'static str> {
    if body["tools"].is_null() {
        return Ok(());
    }
    let tools = body["tools"].as_array().ok_or("tools must be an array")?;
    for tool in tools {
        let object = tool.as_object().ok_or("tool must be an object")?;
        match surface {
            Surface::Chat | Surface::Responses if !tool["type"].is_string() => {
                return Err("tool type is required")
            }
            Surface::Chat | Surface::Responses if tool["type"] == "function" => {
                let function = if tool["function"].is_object() {
                    &tool["function"]
                } else {
                    tool
                };
                if function["name"].as_str().is_none_or(str::is_empty) {
                    return Err("function name is required");
                }
            }
            Surface::Anthropic if !tool["name"].is_string() => return Err("tool name is required"),
            Surface::Gemini => {
                if object.is_empty() {
                    return Err("Gemini tool must not be empty");
                }
                for name in ["functionDeclarations", "function_declarations"] {
                    if let Some(value) = object.get(name) {
                        let declarations = value
                            .as_array()
                            .ok_or("function declarations must be an array")?;
                        if declarations.iter().any(|declaration| {
                            declaration["name"].as_str().is_none_or(str::is_empty)
                        }) {
                            return Err("function name is required");
                        }
                    }
                }
            }
            _ => (),
        }
    }
    Ok(())
}

fn same_native_protocol(surface: Surface, kind: &str) -> bool {
    match surface {
        Surface::Chat | Surface::Responses => matches!(kind, "openai" | "openai_compat" | "grok"),
        Surface::Anthropic => kind == "anthropic",
        Surface::Gemini => kind == "gemini",
    }
}
fn upstream_error(app: &App, err: anyhow::Error) -> Response {
    app.metrics.failures.fetch_add(1, Ordering::Relaxed);
    tracing::warn!(error = %err, "upstream request failed");
    if err.to_string() == "CNB requires an authenticated session (NOT_LOGIN)" {
        return error(
            StatusCode::BAD_GATEWAY,
            "CNB requires an authenticated session; configure CNB_SESSION_COOKIE",
        );
    }
    let status = err
        .downcast_ref::<UpstreamStatus>()
        .map(|s| StatusCode::from_u16(s.0.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY))
        .filter(|s| s.is_client_error() || s.is_server_error())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    error(status, "upstream request failed")
}

async fn handle(
    app: App,
    headers: HeaderMap,
    body: Value,
    surface: Surface,
    path_model: Option<String>,
    stream_override: Option<bool>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }
    if !body.is_object() {
        return error(StatusCode::BAD_REQUEST, "body must be an object");
    }
    if body["n"].as_u64().is_some_and(|n| n != 1) {
        return error(StatusCode::BAD_REQUEST, "only n=1 is supported");
    }
    if surface == Surface::Responses {
        if !body["previous_response_id"].is_null() && !body["previous_response_id"].is_string() {
            return error(
                StatusCode::BAD_REQUEST,
                "previous_response_id must be a string",
            );
        }
        if !body["store"].is_null() && !body["store"].is_boolean() {
            return error(StatusCode::BAD_REQUEST, "store must be a boolean");
        }
        if !body["conversation"].is_null() {
            return error(
                StatusCode::BAD_REQUEST,
                "conversation objects are unsupported",
            );
        }
        if !body["background"].is_null() && !body["background"].is_boolean() {
            return error(StatusCode::BAD_REQUEST, "background must be a boolean");
        }
        if body["background"].as_bool().unwrap_or(false) {
            return error(
                StatusCode::BAD_REQUEST,
                "background responses are unsupported",
            );
        }
    }
    if let Err(message) = validate_tools(&body, surface) {
        return error(StatusCode::BAD_REQUEST, message);
    }
    let is_stream = stream_override.unwrap_or_else(|| body["stream"].as_bool().unwrap_or(false));
    if is_stream && !app.cfg.features.enable_streaming {
        return error(
            StatusCode::BAD_REQUEST,
            "streaming disabled by server config",
        );
    }
    let request_model = match path_model
        .as_deref()
        .or_else(|| body["model"].as_str())
        .filter(|s| !s.trim().is_empty())
    {
        Some(model) => model,
        None => return error(StatusCode::BAD_REQUEST, "model is required"),
    };
    let (resolved_upstream, resolved_model) = match app.cfg.resolve(request_model) {
        Ok(value) => value,
        Err(_) => return error(StatusCode::NOT_FOUND, "no upstream for model"),
    };
    let owner = key(&headers).unwrap_or("");
    let previous = if surface == Surface::Responses {
        if let Some(id) = body["previous_response_id"].as_str() {
            match app.response_store.get(id, owner) {
                Some((_, history)) => Some(history),
                None => return error(StatusCode::NOT_FOUND, "previous response not found"),
            }
        } else {
            None
        }
    } else {
        None
    };
    let has_native_history = matches!(previous, Some(History::Native { .. }));
    if has_native_tool(&body, surface) || has_native_history {
        let header_mode = headers
            .get("x-toolforge-fc-mode")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(&app.cfg.features.fc_mode);
        if !same_native_protocol(surface, &resolved_upstream.kind)
            || !resolved_upstream.native_fc
            || matches!(
                header_mode.to_ascii_lowercase().as_str(),
                "force_prompt" | "prompt"
            )
        {
            return error(
                StatusCode::BAD_REQUEST,
                "native non-function tools require a matching native upstream",
            );
        }
        let mut forward = body.clone();
        if surface != Surface::Gemini {
            forward["model"] = json!(if app.cfg.features.model_passthrough {
                request_model
            } else {
                &resolved_model
            });
        }
        let response_state = if surface == Surface::Responses {
            let mut items = match previous {
                Some(History::Canonical(messages)) => match model::responses_history(&messages) {
                    Ok(items) => items,
                    Err(message) => return error(StatusCode::BAD_REQUEST, message),
                },
                Some(History::Native { items, upstream }) if upstream == resolved_upstream.name => {
                    items
                }
                Some(History::Native { .. }) => {
                    return error(
                        StatusCode::BAD_REQUEST,
                        "native response history requires its original upstream",
                    )
                }
                None => Vec::new(),
            };
            match &body["input"] {
                Value::String(input) => items.push(json!({"role":"user","content":input})),
                Value::Array(input) => items.extend(input.iter().cloned()),
                _ => return error(StatusCode::BAD_REQUEST, "input is required"),
            }
            forward["input"] = json!(items);
            forward["store"] = json!(false);
            forward
                .as_object_mut()
                .unwrap()
                .remove("previous_response_id");
            let enabled = body["store"].as_bool().unwrap_or(true);
            if enabled {
                if forward["include"].is_null() {
                    forward["include"] = json!([]);
                }
                let Some(include) = forward["include"].as_array_mut() else {
                    return error(StatusCode::BAD_REQUEST, "include must be an array");
                };
                if !include
                    .iter()
                    .any(|item| item == "reasoning.encrypted_content")
                {
                    include.push(json!("reasoning.encrypted_content"));
                }
            }
            Some(PendingNativeResponse {
                store: app.response_store.clone(),
                owner: owner.to_owned(),
                items,
                upstream: resolved_upstream.name.clone(),
                previous_id: body["previous_response_id"].clone(),
                enabled,
            })
        } else {
            None
        };
        let client_key = if app.cfg.features.key_passthrough {
            key(&headers)
        } else {
            None
        };
        app.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let native_model = if app.cfg.features.model_passthrough {
            request_model
        } else {
            &resolved_model
        };
        let upstream = match app
            .transport
            .send_native(
                resolved_upstream,
                native_model,
                &forward,
                is_stream,
                client_key,
                surface == Surface::Responses,
            )
            .await
        {
            Ok(response) => response,
            Err(err) => return upstream_error(&app, err),
        };
        let response = if let Some(state) = response_state {
            streaming::native_responses(upstream, state, app.metrics.failures.clone()).await
        } else {
            streaming::native_passthrough(upstream)
        };
        return with_mode(response, Mode::Native);
    }
    let mut req = match model::decode(
        &body,
        surface,
        path_model.as_deref(),
        is_stream,
        app.cfg.features.convert_developer_to_system,
    ) {
        Ok(r) => r,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    let response_state = if surface == Surface::Responses {
        let owner = key(&headers).unwrap_or("").to_owned();
        let mut history = match previous {
            Some(History::Canonical(messages)) => messages,
            Some(History::Native { .. }) => unreachable!(),
            None => Vec::new(),
        };
        let has_instructions = body["instructions"].as_str().is_some_and(|s| !s.is_empty());
        let instruction = if has_instructions {
            Some(req.messages.remove(0))
        } else {
            None
        };
        history.extend(req.messages);
        req.messages = history.clone();
        if let Some(instruction) = instruction {
            req.messages.insert(0, instruction);
        }
        Some(PendingResponse {
            store: app.response_store.clone(),
            owner,
            messages: history,
            previous_id: body["previous_response_id"].clone(),
            enabled: body["store"].as_bool().unwrap_or(true),
        })
    } else {
        None
    };
    let (up, mut up_model) = match app.cfg.resolve(&req.model) {
        Ok(v) => v,
        Err(_) => return error(StatusCode::NOT_FOUND, "no upstream for model"),
    };
    if up.kind == "cnb" && !up.models.iter().any(|m| m == "*" || m == &up_model) {
        if let Some(default) = up.models.first() {
            up_model = default.clone();
        }
    }
    let header_mode = headers
        .get("x-toolforge-fc-mode")
        .and_then(|v| v.to_str().ok());
    let mode = wire::mode(&req, up, &app.cfg.features, header_mode);
    let require_call = req.choice == "required" || req.choice["function"]["name"].is_string();
    if require_call && req.tools.is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "tool_choice requires at least one tool",
        );
    }
    if mode == Mode::Prompt {
        if let Some(name) = req.choice["function"]["name"].as_str() {
            req.tools.retain(|tool| tool.name == name);
            if req.tools.is_empty() {
                return error(
                    StatusCode::BAD_REQUEST,
                    "tool_choice references an unknown tool",
                );
            }
        }
    }
    let (prompt_tools, names) = if mode == Mode::Prompt && app.cfg.features.obfuscate_tool_names {
        fc::obfuscate(&req.tools)
    } else {
        (req.tools.clone(), HashMap::new())
    };
    if !names.is_empty() {
        for m in &mut req.messages {
            for c in &mut m.calls {
                if let Some((safe, _)) = names.iter().find(|(_, original)| *original == &c.name) {
                    c.name = safe.clone();
                }
            }
        }
    }
    let wire = match wire::build(&req, up, &up_model, mode, &app.cfg.features, &prompt_tools) {
        Ok(wire) => wire,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    app.metrics.requests.fetch_add(1, Ordering::Relaxed);
    let client_key = if app.cfg.features.key_passthrough {
        key(&headers)
    } else {
        None
    };
    let response = match app
        .transport
        .send(up, &up_model, &wire, is_stream, client_key)
        .await
    {
        Ok(r) => r,
        Err(e) => return upstream_error(&app, e),
    };
    if is_stream {
        let result = streaming::respond(
            response,
            streaming::Options {
                kind: up.kind.clone(),
                surface,
                model: req.model.clone(),
                mode,
                tools: prompt_tools,
                protocol: app.cfg.features.inject_protocol.clone(),
                strip_think: app.cfg.features.strip_think_tags,
                names,
                failures: app.metrics.failures.clone(),
                require_call,
                response_state,
            },
        );
        return with_mode(result, mode);
    }
    let mut answer = match streaming::collect(
        response,
        &up.kind,
        mode,
        &prompt_tools,
        &app.cfg.features.inject_protocol,
        app.cfg.features.strip_think_tags,
        &names,
    )
    .await
    {
        Ok(a) => a,
        Err(e) => return upstream_error(&app, e),
    };
    if mode == Mode::Prompt && app.cfg.features.enable_fc_error_retry {
        for _ in 0..app.cfg.features.fc_error_retry_max.min(10) {
            if !answer.calls.is_empty() || !fc::looks_like_failed_call(&answer.content) {
                break;
            }
            app.metrics.retries.fetch_add(1, Ordering::Relaxed);
            let mut retry: Request = req.clone();
            retry.messages.push(Message {
                role: "assistant".into(),
                content: json!(answer.content),
                ..Default::default()
            });
            retry.messages.push(Message { role:"user".into(), content:json!("Re-emit the complete valid tool call protocol block for the listed tools, without commentary."), ..Default::default() });
            let wire = match wire::build(
                &retry,
                up,
                &up_model,
                mode,
                &app.cfg.features,
                &prompt_tools,
            ) {
                Ok(wire) => wire,
                Err(message) => return error(StatusCode::BAD_REQUEST, message),
            };
            let response = match app
                .transport
                .send(up, &up_model, &wire, false, client_key)
                .await
            {
                Ok(r) => r,
                Err(e) => return upstream_error(&app, e),
            };
            answer = match streaming::collect(
                response,
                &up.kind,
                mode,
                &prompt_tools,
                &app.cfg.features.inject_protocol,
                app.cfg.features.strip_think_tags,
                &names,
            )
            .await
            {
                Ok(a) => a,
                Err(e) => return upstream_error(&app, e),
            };
        }
    }
    if mode == Mode::Prompt
        && answer.calls.is_empty()
        && fc::looks_like_failed_call(&answer.content)
    {
        return upstream_error(
            &app,
            anyhow::anyhow!("upstream returned an invalid tool call"),
        );
    }
    if require_call && answer.calls.is_empty() {
        return upstream_error(
            &app,
            anyhow::anyhow!("upstream omitted the required tool call"),
        );
    }
    let mut result = model::encode(&req.model, surface, &answer);
    if let Some(state) = response_state {
        if state.finish(&mut result, &answer).is_err() {
            return error(
                StatusCode::INSUFFICIENT_STORAGE,
                "response exceeds the state storage limit",
            );
        }
    }
    with_mode(Json(result).into_response(), mode)
}
