use crate::{
    fc,
    model::{self, Answer, Call, Surface, Tool},
    response_store::{PendingNativeResponse, PendingResponse},
    upstream::UpstreamResponse,
    wire::Mode,
};
use anyhow::{bail, Result};
use async_stream::stream;
use axum::{
    body::Body,
    http::header,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    convert::Infallible,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

const MAX_OUTPUT: usize = 16 * 1024 * 1024;
fn bounded_events(
    response: reqwest::Response,
) -> Pin<Box<dyn Stream<Item = Result<Bytes, anyhow::Error>> + Send>> {
    Box::pin(async_stream::try_stream! {
        let mut input=response.bytes_stream(); let mut event_size=0usize; let mut line_has_content=false;
        while let Some(chunk)=input.next().await {
            let chunk=chunk?;
            for b in &chunk {
                event_size=event_size.saturating_add(1);
                if event_size > MAX_OUTPUT { Err(anyhow::anyhow!("upstream SSE event exceeds limit"))?; }
                if *b == b'\n' { if !line_has_content { event_size=0; } line_has_content=false; }
                else if *b != b'\r' { line_has_content=true; }
            }
            yield chunk;
        }
    })
}
#[derive(Default)]
struct Parts {
    id: String,
    name: String,
    arguments: String,
    input: Value,
}
#[derive(Default)]
struct Collector {
    answer: Answer,
    calls: BTreeMap<usize, Parts>,
    done: bool,
    saw_finish: bool,
}

impl Collector {
    fn consume(&mut self, data: &str, kind: &str) -> Result<(String, String)> {
        if data == "[DONE]" {
            self.done = true;
            return Ok((String::new(), String::new()));
        }
        if data.trim().is_empty() {
            return Ok((String::new(), String::new()));
        }
        let v: Value = serde_json::from_str(data)?;
        if !v["error"].is_null() || v["type"] == "error" {
            bail!("upstream stream returned an error");
        }
        let mut content = String::new();
        let mut reasoning = String::new();
        match kind {
            "anthropic" => match v["type"].as_str().unwrap_or("") {
                "message_start" => {
                    let u = &v["message"]["usage"];
                    self.answer.usage = json!({"prompt_tokens":u["input_tokens"].as_u64().unwrap_or(0),"completion_tokens":u["output_tokens"].as_u64().unwrap_or(0)});
                }
                "content_block_start" => {
                    let i = v["index"].as_u64().unwrap_or(0) as usize;
                    let b = &v["content_block"];
                    if b["type"] == "text" {
                        content = b["text"].as_str().unwrap_or("").into();
                    }
                    if b["type"] == "tool_use" {
                        let p = self.calls.entry(i).or_default();
                        p.id = b["id"].as_str().unwrap_or("").into();
                        p.name = b["name"].as_str().unwrap_or("").into();
                        p.input = b["input"].clone();
                    }
                }
                "content_block_delta" => {
                    let d = &v["delta"];
                    let i = v["index"].as_u64().unwrap_or(0) as usize;
                    if d["type"] == "text_delta" {
                        content = d["text"].as_str().unwrap_or("").into();
                    }
                    if d["type"] == "thinking_delta" {
                        reasoning = d["thinking"].as_str().unwrap_or("").into();
                    }
                    if d["type"] == "input_json_delta" {
                        self.calls
                            .entry(i)
                            .or_default()
                            .arguments
                            .push_str(d["partial_json"].as_str().unwrap_or(""));
                    }
                }
                "message_delta" => {
                    self.saw_finish = true;
                    let stop = v["delta"]["stop_reason"].as_str().unwrap_or("");
                    self.answer.finish = if stop == "max_tokens" {
                        "length"
                    } else if stop == "tool_use" {
                        "tool_calls"
                    } else {
                        "stop"
                    }
                    .into();
                    if let Some(n) = v["usage"]["output_tokens"].as_u64() {
                        self.answer.usage["completion_tokens"] = json!(n);
                    }
                }
                "message_stop" => self.done = true,
                _ => (),
            },
            "gemini" => {
                let candidate = &v["candidates"][0];
                if let Some(reason) = candidate["finishReason"].as_str() {
                    self.saw_finish = true;
                    self.answer.finish = if reason == "MAX_TOKENS" {
                        "length"
                    } else {
                        "stop"
                    }
                    .into();
                }
                for p in candidate["content"]["parts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    if let Some(s) = p["text"].as_str() {
                        content.push_str(s);
                    }
                    if let Some(f) = p.get("functionCall").or_else(|| p.get("function_call")) {
                        let i = self.calls.len();
                        self.calls.insert(
                            i,
                            Parts {
                                id: String::new(),
                                name: f["name"].as_str().unwrap_or("").into(),
                                input: f["args"].clone(),
                                arguments: String::new(),
                            },
                        );
                    }
                }
                if v["usageMetadata"].is_object() {
                    let u = &v["usageMetadata"];
                    self.answer.usage = json!({"prompt_tokens":u["promptTokenCount"].as_u64().unwrap_or(0),"completion_tokens":u["candidatesTokenCount"].as_u64().unwrap_or(0),"total_tokens":u["totalTokenCount"].as_u64().unwrap_or(0)});
                }
            }
            _ => {
                if v["usage"].is_object() {
                    self.answer.usage = v["usage"].clone();
                }
                let choice = &v["choices"][0];
                let delta = &choice["delta"];
                content = delta["content"].as_str().unwrap_or("").into();
                reasoning = delta["reasoning_content"].as_str().unwrap_or("").into();
                if let Some(reason) = choice["finish_reason"].as_str() {
                    self.saw_finish = true;
                    self.answer.finish = reason.into();
                }
                for c in delta["tool_calls"].as_array().into_iter().flatten() {
                    let i = c["index"].as_u64().unwrap_or(0) as usize;
                    let p = self.calls.entry(i).or_default();
                    if let Some(s) = c["id"].as_str() {
                        p.id.push_str(s);
                    }
                    if let Some(s) = c["function"]["name"].as_str() {
                        p.name.push_str(s);
                    }
                    if let Some(s) = c["function"]["arguments"].as_str() {
                        p.arguments.push_str(s);
                    }
                }
            }
        }
        self.answer.content.push_str(&content);
        self.answer.reasoning.push_str(&reasoning);
        let call_bytes: usize = self.calls.values().map(|p| p.arguments.len()).sum();
        if self
            .answer
            .content
            .len()
            .saturating_add(self.answer.reasoning.len())
            .saturating_add(call_bytes)
            > MAX_OUTPUT
        {
            bail!("upstream output exceeds limit");
        }
        Ok((content, reasoning))
    }
    fn finish(
        mut self,
        mode: Mode,
        tools: &[Tool],
        protocol: &str,
        strip_think: bool,
        names: &HashMap<String, String>,
    ) -> Result<Answer> {
        if !self.done && !self.saw_finish {
            bail!("upstream stream ended before completion");
        }
        for (_, p) in self.calls {
            let args = if p.arguments.is_empty() {
                if p.input.is_object() {
                    p.input
                } else {
                    json!({})
                }
            } else {
                serde_json::from_str::<Value>(&p.arguments)
                    .map_err(|_| anyhow::anyhow!("invalid streamed tool arguments"))?
            };
            self.answer.calls.push(Call {
                id: p.id,
                name: p.name,
                args,
            });
        }
        if mode == Mode::Prompt && self.answer.finish != "length" {
            let parsed = fc::parse(&self.answer.content, tools, protocol, strip_think);
            if !parsed.is_empty() {
                self.answer.calls = parsed;
                self.answer.content.clear();
            }
        }
        for c in &mut self.answer.calls {
            if let Some(original) = names.get(&c.name) {
                c.name = original.clone();
            }
            if c.id.is_empty() {
                c.id = model::id("call_");
            }
        }
        if !self.answer.calls.is_empty() {
            self.answer.finish = "tool_calls".into();
        }
        Ok(self.answer)
    }
}

pub async fn collect(
    mut upstream: UpstreamResponse,
    kind: &str,
    mode: Mode,
    tools: &[Tool],
    protocol: &str,
    strip_think: bool,
    names: &HashMap<String, String>,
) -> Result<Answer> {
    if !upstream.is_sse {
        let mut chunks = upstream.response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk?;
            if bytes.len().saturating_add(chunk.len()) > MAX_OUTPUT {
                bail!("upstream output exceeds limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes)?;
        let mut answer = crate::wire::decode_result(&value, kind);
        if mode == Mode::Prompt && answer.finish != "length" {
            let parsed = fc::parse(&answer.content, tools, protocol, strip_think);
            if !parsed.is_empty() {
                answer.content.clear();
                answer.calls = parsed;
                answer.finish = "tool_calls".into();
            }
        }
        for c in &mut answer.calls {
            if let Some(original) = names.get(&c.name) {
                c.name = original.clone();
            }
            if c.id.is_empty() {
                c.id = model::id("call_");
            }
        }
        return Ok(answer);
    }
    let mut collector = Collector::default();
    let mut events = bounded_events(upstream.response).eventsource();
    while let Some(event) = events.next().await {
        match event {
            Ok(event) => {
                if let Err(e) = collector.consume(&event.data, kind) {
                    if let Some(lease) = upstream.lease.as_mut() {
                        lease.fail();
                    }
                    return Err(e);
                }
            }
            Err(_) => {
                if let Some(lease) = upstream.lease.as_mut() {
                    lease.fail();
                }
                bail!("upstream stream interrupted");
            }
        }
        if collector.done {
            break;
        }
    }
    let result = collector.finish(mode, tools, protocol, strip_think, names);
    if result.is_err() {
        if let Some(lease) = upstream.lease.as_mut() {
            lease.fail();
        }
    }
    result
}

fn frame(event: Option<&str>, data: &Value) -> Bytes {
    Bytes::from(format!(
        "{}data: {}\n\n",
        event.map(|s| format!("event: {s}\n")).unwrap_or_default(),
        data
    ))
}
struct Encoder {
    surface: Surface,
    model: String,
    id: String,
    item_id: String,
    created: u64,
    text_open: bool,
    text: String,
    items: Vec<Value>,
    completed_response: Option<Value>,
}
impl Encoder {
    fn new(surface: Surface, model: String) -> Self {
        Self {
            surface,
            model,
            id: model::id(match surface {
                Surface::Chat => "chatcmpl-",
                Surface::Anthropic => "msg_",
                Surface::Responses => "resp_",
                Surface::Gemini => "gen_",
            }),
            item_id: model::id("msg_"),
            created: model::now(),
            text_open: false,
            text: String::new(),
            items: Vec::new(),
            completed_response: None,
        }
    }
    fn start(&self) -> Vec<Bytes> {
        match self.surface {
            Surface::Chat => vec![frame(
                None,
                &json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,"choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}),
            )],
            Surface::Anthropic => vec![frame(
                Some("message_start"),
                &json!({"type":"message_start","message":{"id":self.id,"type":"message","role":"assistant","model":self.model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}}),
            )],
            Surface::Responses => vec![
                frame(
                    Some("response.created"),
                    &json!({"type":"response.created","response":{"id":self.id,"object":"response","created_at":self.created,"model":self.model,"status":"in_progress","output":[]}}),
                ),
                frame(
                    Some("response.in_progress"),
                    &json!({"type":"response.in_progress","response":{"id":self.id,"object":"response","created_at":self.created,"model":self.model,"status":"in_progress","output":[]}}),
                ),
            ],
            Surface::Gemini => Vec::new(),
        }
    }
    fn delta(&mut self, text: &str, reasoning: &str) -> Vec<Bytes> {
        let mut out = Vec::new();
        if !text.is_empty() {
            if !self.text_open {
                match self.surface {
                    Surface::Anthropic => out.push(frame(Some("content_block_start"),&json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}))),
                    Surface::Responses => {
                        out.push(frame(Some("response.output_item.added"),&json!({"type":"response.output_item.added","output_index":0,"item":{"id":self.item_id,"type":"message","status":"in_progress","role":"assistant","content":[]}})));
                        out.push(frame(Some("response.content_part.added"),&json!({"type":"response.content_part.added","item_id":self.item_id,"output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}})));
                    }
                    _ => (),
                }
                self.text_open = true;
            }
            self.text.push_str(text);
            out.push(match self.surface {
                Surface::Chat=>frame(None,&json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]})),
                Surface::Anthropic=>frame(Some("content_block_delta"),&json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}})),
                Surface::Responses=>frame(Some("response.output_text.delta"),&json!({"type":"response.output_text.delta","item_id":self.item_id,"output_index":0,"content_index":0,"delta":text})),
                Surface::Gemini=>frame(None,&json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":text}]}}],"modelVersion":self.model})),
            });
        }
        if !reasoning.is_empty() && self.surface == Surface::Chat {
            out.push(frame(None,&json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,"choices":[{"index":0,"delta":{"reasoning_content":reasoning},"finish_reason":null}]})));
        }
        out
    }
    fn end(&mut self, answer: &Answer, response_state: Option<&PendingResponse>) -> Vec<Bytes> {
        let mut out = Vec::new();
        if self.text_open {
            if self.surface == Surface::Anthropic {
                out.push(frame(
                    Some("content_block_stop"),
                    &json!({"type":"content_block_stop","index":0}),
                ));
            }
            if self.surface == Surface::Responses {
                let part = json!({"type":"output_text","text":self.text,"annotations":[]});
                out.push(frame(Some("response.output_text.done"),&json!({"type":"response.output_text.done","item_id":self.item_id,"output_index":0,"content_index":0,"text":self.text})));
                out.push(frame(Some("response.content_part.done"),&json!({"type":"response.content_part.done","item_id":self.item_id,"output_index":0,"content_index":0,"part":part})));
                let item = json!({"id":self.item_id,"type":"message","status":"completed","role":"assistant","content":[part]});
                out.push(frame(
                    Some("response.output_item.done"),
                    &json!({"type":"response.output_item.done","output_index":0,"item":item}),
                ));
                self.items.push(item);
            }
        }
        for (i, c) in answer.calls.iter().enumerate() {
            match self.surface {
                Surface::Chat => out.push(frame(None,&json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,"choices":[{"index":0,"delta":{"tool_calls":[{"index":i,"id":c.id,"type":"function","function":{"name":c.name,"arguments":c.args.to_string()}}]},"finish_reason":null}]}))),
                Surface::Anthropic => {
                    let index=i+usize::from(self.text_open);
                    out.push(frame(Some("content_block_start"),&json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":c.id,"name":c.name,"input":{}}})));
                    out.push(frame(Some("content_block_delta"),&json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":c.args.to_string()}})));
                    out.push(frame(Some("content_block_stop"),&json!({"type":"content_block_stop","index":index})));
                }
                Surface::Responses => {
                    let idx=i+usize::from(self.text_open); let item_id=model::id("fc_"); let args=c.args.to_string();
                    out.push(frame(Some("response.output_item.added"),&json!({"type":"response.output_item.added","output_index":idx,"item":{"id":item_id,"type":"function_call","call_id":c.id,"name":c.name,"arguments":"","status":"in_progress"}})));
                    out.push(frame(Some("response.function_call_arguments.delta"),&json!({"type":"response.function_call_arguments.delta","item_id":item_id,"output_index":idx,"delta":args})));
                    out.push(frame(Some("response.function_call_arguments.done"),&json!({"type":"response.function_call_arguments.done","item_id":item_id,"output_index":idx,"arguments":args})));
                    let item=json!({"id":item_id,"type":"function_call","call_id":c.id,"name":c.name,"arguments":args,"status":"completed"});
                    out.push(frame(Some("response.output_item.done"),&json!({"type":"response.output_item.done","output_index":idx,"item":item})));
                    self.items.push(item);
                }
                Surface::Gemini => out.push(frame(None,&json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"functionCall":{"name":c.name,"args":c.args}}]}}],"modelVersion":self.model}))),
            }
        }
        match self.surface {
            Surface::Chat => { out.push(frame(None,&json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,"choices":[{"index":0,"delta":{},"finish_reason":if answer.calls.is_empty() { if answer.finish.is_empty() {"stop"} else {&answer.finish} } else {"tool_calls"}}]}))); out.push(Bytes::from_static(b"data: [DONE]\n\n")); }
            Surface::Anthropic => { out.push(frame(Some("message_delta"),&json!({"type":"message_delta","delta":{"stop_reason":if !answer.calls.is_empty() {"tool_use"} else if answer.finish == "length" {"max_tokens"} else {"end_turn"},"stop_sequence":null},"usage":{"output_tokens":answer.usage["completion_tokens"].as_u64().unwrap_or(0)}}))); out.push(frame(Some("message_stop"),&json!({"type":"message_stop"}))); }
            Surface::Responses => {
                let mut result=model::encode(&self.model,Surface::Responses,answer);
                result["id"]=json!(self.id);
                result["created_at"]=json!(self.created);
                result["output"]=json!(self.items);
                if let Some(state) = response_state {
                    result["previous_response_id"] = state.previous_id.clone();
                    result["store"] = json!(state.enabled);
                }
                self.completed_response = Some(result.clone());
                out.push(frame(Some("response.completed"),&json!({"type":"response.completed","response":result})));
            }
            Surface::Gemini => out.push(frame(None,&json!({"candidates":[{"index":0,"finishReason":if answer.finish == "length" {"MAX_TOKENS"} else {"STOP"}}],"modelVersion":self.model}))),
        }
        out
    }
}

pub struct Options {
    pub kind: String,
    pub surface: Surface,
    pub model: String,
    pub mode: Mode,
    pub tools: Vec<Tool>,
    pub protocol: String,
    pub strip_think: bool,
    pub names: HashMap<String, String>,
    pub failures: Arc<AtomicU64>,
    pub require_call: bool,
    pub response_state: Option<PendingResponse>,
}

pub fn respond(mut upstream: UpstreamResponse, options: Options) -> Response {
    let Options {
        kind,
        surface,
        model,
        mode,
        tools,
        protocol,
        strip_think,
        names,
        failures,
        require_call,
        response_state,
    } = options;
    let output = stream! {
        let mut encoder=Encoder::new(surface,model); for b in encoder.start() { yield Ok::<Bytes,Infallible>(b); }
        let mut collector=Collector::default(); let mut events=bounded_events(upstream.response).eventsource(); let mut failed=false;
        while let Some(event)=events.next().await {
            match event {
                Ok(event) => match collector.consume(&event.data,&kind) {
                    Ok((text,reasoning)) => { if mode != Mode::Prompt { for b in encoder.delta(&text,&reasoning) { yield Ok(b); } } }
                    Err(_) => { failed=true; break; }
                },
                Err(_) => { failed=true; break; }
            }
            if collector.done { break; }
        }
        let result=if failed { Err(anyhow::anyhow!("upstream stream interrupted")) } else { collector.finish(mode,&tools,&protocol,strip_think,&names).and_then(|answer| {
            if answer.calls.is_empty() && (require_call || (mode == Mode::Prompt && fc::looks_like_failed_call(&answer.content))) { Err(anyhow::anyhow!("invalid or missing tool call")) } else { Ok(answer) }
        }) };
        match result {
            Ok(answer) => {
                if mode == Mode::Prompt && !answer.content.is_empty() { for b in encoder.delta(&answer.content,"") { yield Ok(b); } }
                let frames = encoder.end(&answer, response_state.as_ref());
                let saved = match (response_state, encoder.completed_response.take()) {
                    (Some(state), Some(mut response)) => state.finish(&mut response, &answer).is_ok(),
                    (Some(_), None) => false,
                    (None, _) => true,
                };
                if saved {
                    for b in frames { yield Ok(b); }
                } else {
                    failures.fetch_add(1,Ordering::Relaxed);
                    yield Ok(frame(Some("error"),&json!({"type":"error","error":{"type":"api_error","message":"response exceeds the state storage limit"}})));
                }
            }
            Err(_) => {
                failures.fetch_add(1,Ordering::Relaxed);
                if let Some(lease)=upstream.lease.as_mut() { lease.fail(); }
                yield Ok(frame(Some("error"),&json!({"type":"error","error":{"type":"api_error","message":"upstream stream interrupted or invalid"}})));
            }
        }
    };
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        Body::from_stream(output),
    )
        .into_response()
}

pub fn native_passthrough(upstream: UpstreamResponse) -> Response {
    let content_type = upstream
        .response
        .headers()
        .get(header::CONTENT_TYPE)
        .cloned();
    let body = if upstream.is_sse {
        Body::from_stream(bounded_events(upstream.response))
    } else {
        Body::from_stream(upstream.response.bytes_stream())
    };
    let mut response = body.into_response();
    if let Some(content_type) = content_type {
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type);
    }
    response
}

fn native_error(status: axum::http::StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(json!({"error":{"type":"api_error","message":message}})),
    )
        .into_response()
}

pub async fn native_responses(
    upstream: UpstreamResponse,
    state: PendingNativeResponse,
    failures: Arc<AtomicU64>,
) -> Response {
    if !upstream.is_sse {
        let mut chunks = upstream.response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = chunks.next().await {
            match chunk {
                Ok(chunk) if bytes.len().saturating_add(chunk.len()) <= MAX_OUTPUT => {
                    bytes.extend_from_slice(&chunk)
                }
                _ => {
                    failures.fetch_add(1, Ordering::Relaxed);
                    return native_error(
                        axum::http::StatusCode::BAD_GATEWAY,
                        "invalid upstream response",
                    );
                }
            }
        }
        let Ok(mut response) = serde_json::from_slice::<Value>(&bytes) else {
            failures.fetch_add(1, Ordering::Relaxed);
            return native_error(
                axum::http::StatusCode::BAD_GATEWAY,
                "invalid upstream response",
            );
        };
        if response["id"].as_str().is_none_or(str::is_empty) || !response["output"].is_array() {
            failures.fetch_add(1, Ordering::Relaxed);
            return native_error(
                axum::http::StatusCode::BAD_GATEWAY,
                "invalid upstream response",
            );
        }
        if state.finish(&mut response).is_err() {
            failures.fetch_add(1, Ordering::Relaxed);
            return native_error(
                axum::http::StatusCode::INSUFFICIENT_STORAGE,
                "response cannot be stored",
            );
        }
        return axum::Json(response).into_response();
    }
    let output = stream! {
        let mut events = bounded_events(upstream.response).eventsource();
        let mut state = Some(state);
        let mut completed = false;
        let mut failed = false;
        while let Some(event) = events.next().await {
            let Ok(event) = event else { failed = true; break; };
            let mut data = event.data.clone();
            if let Ok(mut value) = serde_json::from_str::<Value>(&data) {
                let kind = value["type"].as_str().unwrap_or("").to_owned();
                if kind == "error" || kind == "response.failed" {
                    failed = true;
                    break;
                }
                if matches!(kind.as_str(), "response.created" | "response.in_progress") {
                    if let Some(pending) = state.as_ref() {
                        if value["response"].is_object() { pending.decorate(&mut value["response"]); }
                    }
                }
                if kind == "response.completed" {
                    let Some(pending) = state.take() else { failed = true; break; };
                    if value["response"]["id"].as_str().is_none_or(str::is_empty)
                        || !value["response"]["output"].is_array()
                        || pending.finish(&mut value["response"]).is_err() {
                        failed = true;
                        break;
                    }
                    completed = true;
                }
                data = value.to_string();
            }
            let mut encoded = String::new();
            if !event.event.is_empty() { encoded.push_str(&format!("event: {}\n", event.event)); }
            if !event.id.is_empty() { encoded.push_str(&format!("id: {}\n", event.id)); }
            if let Some(retry) = event.retry { encoded.push_str(&format!("retry: {}\n", retry.as_millis())); }
            for line in data.split('\n') { encoded.push_str("data: "); encoded.push_str(line); encoded.push('\n'); }
            encoded.push('\n');
            yield Ok::<Bytes, Infallible>(Bytes::from(encoded));
        }
        if failed || !completed {
            failures.fetch_add(1, Ordering::Relaxed);
            yield Ok(frame(Some("error"), &json!({"type":"error","error":{"type":"api_error","message":"upstream stream interrupted or invalid"}})));
        }
    };
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(output),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_openai_tool_arguments_are_reassembled() {
        let mut c = Collector::default();
        c.consume(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_x","function":{"name":"read","arguments":"{\"path\":"}}]}}]}"#,"openai").unwrap();
        c.consume(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]},"finish_reason":"tool_calls"}]}"#,"openai").unwrap();
        let a = c
            .finish(Mode::Native, &[], "XYML", true, &HashMap::new())
            .unwrap();
        assert_eq!(a.calls[0].id, "call_x");
        assert_eq!(a.calls[0].args["path"], "a.txt");
    }
}
