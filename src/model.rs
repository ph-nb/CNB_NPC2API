use crate::multimodal;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    Chat,
    Anthropic,
    Gemini,
    Responses,
}

#[derive(Clone, Debug)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub schema: Value,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct Call {
    pub id: String,
    pub name: String,
    pub args: Value,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct Message {
    pub role: String,
    pub content: Value,
    pub calls: Vec<Call>,
    pub call_id: String,
    pub name: String,
}
#[derive(Clone, Debug)]
pub struct Request {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    pub choice: Value,
    pub stream: bool,
    pub extra: Value,
}
#[derive(Clone, Debug, Default)]
pub struct Answer {
    pub content: String,
    pub reasoning: String,
    pub calls: Vec<Call>,
    pub finish: String,
    pub usage: Value,
}

pub fn id(prefix: &str) -> String {
    format!("{}{}", prefix, Uuid::new_v4().simple())
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn as_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().or_else(|| v.get("text").and_then(Value::as_str)))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
pub fn parse_args(value: &Value) -> Value {
    match value {
        Value::Object(_) => value.clone(),
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Object(m)) => Value::Object(m),
            Ok(v) => json!({"value":v}),
            Err(_) if !s.is_empty() => json!({"_raw":s}),
            Err(_) => json!({}),
        },
        Value::Null => json!({}),
        v => json!({"value":v}),
    }
}
fn message(role: &str, content: Value) -> Message {
    Message {
        role: role.into(),
        content,
        ..Default::default()
    }
}
fn call_from_openai(v: &Value) -> Call {
    let f = &v["function"];
    Call {
        id: v["id"].as_str().unwrap_or("").into(),
        name: f["name"].as_str().unwrap_or("").into(),
        args: parse_args(&f["arguments"]),
    }
}
fn push_tool(tools: &mut Vec<Tool>, v: &Value) {
    let f = if v["function"].is_object() {
        &v["function"]
    } else {
        v
    };
    let Some(name) = f["name"].as_str().filter(|s| !s.is_empty()) else {
        return;
    };
    let schema = f
        .get("parameters")
        .or_else(|| f.get("input_schema"))
        .cloned()
        .unwrap_or_else(|| json!({"type":"object","properties":{}}));
    tools.push(Tool {
        name: name.into(),
        description: f["description"].as_str().unwrap_or("").into(),
        schema,
    });
}

pub fn decode(
    body: &Value,
    surface: Surface,
    path_model: Option<&str>,
    stream: bool,
    convert_developer: bool,
) -> Result<Request, &'static str> {
    let model = path_model
        .or_else(|| body["model"].as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or("model is required")?
        .to_owned();
    let mut messages = Vec::new();
    let mut tools = Vec::new();
    match surface {
        Surface::Chat => {
            for v in body["messages"].as_array().ok_or("messages is required")? {
                let role = v["role"].as_str().unwrap_or("user");
                let role = if role == "developer" && convert_developer {
                    "system"
                } else {
                    role
                };
                let mut m = message(role, multimodal::content(&v["content"], surface)?);
                m.name = v["name"].as_str().unwrap_or("").into();
                m.call_id = v["tool_call_id"].as_str().unwrap_or("").into();
                if let Some(calls) = v["tool_calls"].as_array() {
                    m.calls = calls.iter().map(call_from_openai).collect();
                }
                messages.push(m);
            }
            if let Some(items) = body
                .get("tools")
                .or_else(|| body.get("functions"))
                .and_then(Value::as_array)
            {
                for item in items {
                    push_tool(&mut tools, item);
                }
            }
        }
        Surface::Anthropic => {
            let sys = as_text(&body["system"]);
            if !sys.is_empty() {
                messages.push(message("system", json!(sys)));
            }
            for v in body["messages"].as_array().ok_or("messages is required")? {
                let role = v["role"].as_str().unwrap_or("user");
                if let Some(blocks) = v["content"].as_array() {
                    let mut m = message(role, json!(""));
                    let mut content = Vec::new();
                    let mut results = Vec::new();
                    for block in blocks {
                        match block["type"].as_str().unwrap_or("") {
                            "text" | "image" => {
                                let one = multimodal::content(&json!([block]), surface)?;
                                content.extend(one.as_array().unwrap().iter().cloned());
                            }
                            "tool_use" => m.calls.push(Call {
                                id: block["id"]
                                    .as_str()
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| id("call_")),
                                name: block["name"].as_str().unwrap_or("").into(),
                                args: parse_args(&block["input"]),
                            }),
                            "tool_result" => {
                                let tool_content = multimodal::content(&block["content"], surface)?;
                                if multimodal::has_image(&tool_content) {
                                    return Err("images in tool results are unsupported");
                                }
                                let mut r = message("tool", json!(as_text(&tool_content)));
                                r.call_id = block["tool_use_id"].as_str().unwrap_or("").into();
                                results.push(r);
                            }
                            _ => return Err("unsupported Anthropic content block"),
                        }
                    }
                    m.content = if content.iter().any(|block| block["type"] == "image_url") {
                        json!(content)
                    } else {
                        json!(content
                            .iter()
                            .filter_map(|block| block["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n"))
                    };
                    if role == "assistant" || !content.is_empty() {
                        messages.push(m);
                    }
                    messages.extend(results);
                } else {
                    messages.push(message(role, v["content"].clone()));
                }
            }
            if let Some(items) = body["tools"].as_array() {
                for item in items {
                    push_tool(&mut tools, item);
                }
            }
        }
        Surface::Responses => {
            if let Some(s) = body["instructions"].as_str().filter(|s| !s.is_empty()) {
                messages.push(message("system", json!(s)));
            }
            match &body["input"] {
                Value::String(s) => messages.push(message("user", json!(s))),
                Value::Array(items) => {
                    for v in items {
                        let kind = v["type"].as_str().unwrap_or("");
                        if kind == "function_call" {
                            let mut m = message("assistant", json!(""));
                            m.calls.push(Call {
                                id: v["call_id"]
                                    .as_str()
                                    .or_else(|| v["id"].as_str())
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| id("call_")),
                                name: v["name"].as_str().unwrap_or("").into(),
                                args: parse_args(&v["arguments"]),
                            });
                            messages.push(m);
                        } else if kind == "function_call_output" {
                            let mut m = message("tool", v["output"].clone());
                            m.call_id = v["call_id"].as_str().unwrap_or("").into();
                            messages.push(m);
                        } else if kind == "message" || v["role"].is_string() {
                            messages.push(message(
                                v["role"].as_str().unwrap_or("user"),
                                multimodal::content(&v["content"], surface)?,
                            ));
                        }
                    }
                }
                _ => return Err("input is required"),
            }
            if let Some(items) = body["tools"].as_array() {
                for item in items {
                    push_tool(&mut tools, item);
                }
            }
        }
        Surface::Gemini => {
            let mut pending_calls: HashMap<String, VecDeque<String>> = HashMap::new();
            let sys = body
                .get("systemInstruction")
                .or_else(|| body.get("system_instruction"))
                .unwrap_or(&Value::Null);
            let sys_text = if sys.is_object() {
                as_text(&sys["parts"])
            } else {
                as_text(sys)
            };
            if !sys_text.is_empty() {
                messages.push(message("system", json!(sys_text)));
            }
            for v in body["contents"].as_array().ok_or("contents is required")? {
                let role = if v["role"].as_str() == Some("model") {
                    "assistant"
                } else {
                    "user"
                };
                let mut m = message(role, json!(""));
                let mut content = Vec::new();
                for p in v["parts"].as_array().into_iter().flatten() {
                    if let Some(s) = p["text"].as_str() {
                        content.push(json!({"type":"text","text":s}));
                    }
                    if let Some(image) = multimodal::from_gemini(p)? {
                        content.push(image);
                    } else if p.get("text").is_none()
                        && p.get("functionCall").is_none()
                        && p.get("function_call").is_none()
                        && p.get("functionResponse").is_none()
                        && p.get("function_response").is_none()
                    {
                        return Err("unsupported Gemini content part");
                    }
                    let f = p.get("functionCall").or_else(|| p.get("function_call"));
                    if let Some(f) = f {
                        let call = Call {
                            id: f["id"]
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| id("call_")),
                            name: f["name"].as_str().unwrap_or("").into(),
                            args: parse_args(&f["args"]),
                        };
                        pending_calls
                            .entry(call.name.clone())
                            .or_default()
                            .push_back(call.id.clone());
                        m.calls.push(call);
                    }
                    let r = p
                        .get("functionResponse")
                        .or_else(|| p.get("function_response"));
                    if let Some(r) = r {
                        let mut result = message("tool", r["response"].clone());
                        result.name = r["name"].as_str().unwrap_or("").into();
                        let pending = pending_calls
                            .get_mut(&result.name)
                            .and_then(VecDeque::pop_front);
                        result.call_id = r["id"]
                            .as_str()
                            .map(str::to_owned)
                            .or(pending)
                            .unwrap_or_default();
                        messages.push(result);
                    }
                }
                m.content = if content.iter().any(|block| block["type"] == "image_url") {
                    json!(content)
                } else {
                    json!(content
                        .iter()
                        .filter_map(|block| block["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"))
                };
                if !content.is_empty() || !m.calls.is_empty() {
                    messages.push(m);
                }
            }
            for group in body["tools"].as_array().into_iter().flatten() {
                if let Some(items) = group
                    .get("functionDeclarations")
                    .or_else(|| group.get("function_declarations"))
                    .and_then(Value::as_array)
                {
                    for item in items {
                        push_tool(&mut tools, item);
                    }
                }
            }
        }
    }
    if messages.is_empty() {
        return Err("no messages provided");
    }
    let mut extra = json!({});
    let source = if surface == Surface::Gemini {
        body.get("generationConfig")
            .or_else(|| body.get("generation_config"))
            .unwrap_or(&Value::Null)
    } else {
        body
    };
    for (from, to) in [
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("topP", "top_p"),
        ("max_tokens", "max_tokens"),
        ("max_completion_tokens", "max_tokens"),
        ("max_output_tokens", "max_tokens"),
        ("maxOutputTokens", "max_tokens"),
        ("stop", "stop"),
        ("presence_penalty", "presence_penalty"),
        ("frequency_penalty", "frequency_penalty"),
        ("seed", "seed"),
        ("response_format", "response_format"),
        ("n", "n"),
    ] {
        if let Some(v) = source.get(from) {
            extra[to] = v.clone();
        }
    }
    let choice = match &body["tool_choice"] {
        v if v["type"] == "tool" => json!({"type":"function","function":{"name":v["name"]}}),
        v if v["type"] == "any" => json!("required"),
        v if v["type"] == "auto" => json!("auto"),
        Value::Null => json!("auto"),
        v => v.clone(),
    };
    Ok(Request {
        model,
        messages,
        tools,
        choice,
        stream,
        extra,
    })
}

pub fn openai_calls(calls: &[Call]) -> Vec<Value> {
    calls.iter().map(|c| json!({"id":if c.id.is_empty() { id("call_") } else { c.id.clone() },"type":"function","function":{"name":c.name,"arguments":c.args.to_string()}})).collect()
}

pub fn responses_history(messages: &[Message]) -> Result<Vec<Value>, &'static str> {
    let mut items = Vec::new();
    for message in messages {
        if message.role == "tool" {
            items.push(json!({"type":"function_call_output","call_id":message.call_id,"output":as_text(&message.content)}));
            continue;
        }
        if !message.content.is_null()
            && (!as_text(&message.content).is_empty() || multimodal::has_image(&message.content))
        {
            items.push(json!({"role":message.role,"content":multimodal::responses(&message.content, &message.role)?}));
        }
        for call in &message.calls {
            items.push(json!({"type":"function_call","call_id":call.id,"name":call.name,"arguments":call.args.to_string()}));
        }
    }
    Ok(items)
}
pub fn encode(model: &str, surface: Surface, answer: &Answer) -> Value {
    let has_calls = !answer.calls.is_empty();
    let prompt = answer.usage["prompt_tokens"].as_u64().unwrap_or(0);
    let completion = answer.usage["completion_tokens"].as_u64().unwrap_or(0);
    match surface {
        Surface::Chat => {
            let mut msg = json!({"role":"assistant","content":if answer.content.is_empty() { Value::Null } else { json!(answer.content) }});
            if !answer.reasoning.is_empty() {
                msg["reasoning_content"] = json!(answer.reasoning);
            }
            if has_calls {
                msg["tool_calls"] = json!(openai_calls(&answer.calls));
            }
            json!({"id":id("chatcmpl-"),"object":"chat.completion","created":now(),"model":model,"choices":[{"index":0,"message":msg,"finish_reason":if has_calls { "tool_calls" } else if answer.finish.is_empty() { "stop" } else { &answer.finish },"logprobs":null}],"usage":{"prompt_tokens":prompt,"completion_tokens":completion,"total_tokens":answer.usage["total_tokens"].as_u64().unwrap_or(prompt+completion)}})
        }
        Surface::Anthropic => {
            let mut content = Vec::new();
            if !answer.content.is_empty() {
                content.push(json!({"type":"text","text":answer.content}));
            }
            for c in &answer.calls {
                content.push(json!({"type":"tool_use","id":if c.id.is_empty() { id("toolu_") } else { c.id.clone() },"name":c.name,"input":c.args}));
            }
            if content.is_empty() {
                content.push(json!({"type":"text","text":""}));
            }
            json!({"id":id("msg_"),"type":"message","role":"assistant","model":model,"content":content,"stop_reason":if has_calls {"tool_use"} else if answer.finish == "length" {"max_tokens"} else {"end_turn"},"stop_sequence":null,"usage":{"input_tokens":prompt,"output_tokens":completion}})
        }
        Surface::Gemini => {
            let mut parts = Vec::new();
            if !answer.content.is_empty() {
                parts.push(json!({"text":answer.content}));
            }
            for c in &answer.calls {
                parts.push(json!({"functionCall":{"name":c.name,"args":c.args}}));
            }
            if parts.is_empty() {
                parts.push(json!({"text":""}));
            }
            json!({"candidates":[{"content":{"role":"model","parts":parts},"finishReason":if answer.finish == "length" {"MAX_TOKENS"} else {"STOP"},"index":0}],"modelVersion":model,"usageMetadata":{"promptTokenCount":prompt,"candidatesTokenCount":completion,"totalTokenCount":answer.usage["total_tokens"].as_u64().unwrap_or(prompt+completion)}})
        }
        Surface::Responses => {
            let mut output = Vec::new();
            if !answer.content.is_empty() {
                output.push(json!({"id":id("msg_"),"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":answer.content,"annotations":[]}]}));
            }
            for c in &answer.calls {
                output.push(json!({"id":id("fc_"),"type":"function_call","call_id":if c.id.is_empty() { id("call_") } else { c.id.clone() },"name":c.name,"arguments":c.args.to_string(),"status":"completed"}));
            }
            json!({"id":id("resp_"),"object":"response","created_at":now(),"model":model,"status":"completed","output":output,"usage":{"input_tokens":prompt,"output_tokens":completion,"total_tokens":answer.usage["total_tokens"].as_u64().unwrap_or(prompt+completion)}})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn anthropic_tool_history_becomes_canonical() {
        let b = json!({"model":"m","messages":[{"role":"assistant","content":[{"type":"tool_use","id":"c","name":"read","input":{"x":1}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":"ok"}]}]});
        let r = decode(&b, Surface::Anthropic, None, false, true).unwrap();
        assert_eq!(r.messages[0].calls[0].id, "c");
        assert_eq!(r.messages[1].call_id, "c");
    }
    #[test]
    fn encodes_tool_call_for_all_surfaces() {
        let a = Answer {
            calls: vec![Call {
                id: "c".into(),
                name: "read".into(),
                args: json!({"x":1}),
            }],
            ..Default::default()
        };
        assert_eq!(
            encode("m", Surface::Chat, &a)["choices"][0]["finish_reason"],
            "tool_calls"
        );
        assert_eq!(encode("m", Surface::Anthropic, &a)["content"][0]["id"], "c");
        assert_eq!(
            encode("m", Surface::Gemini, &a)["candidates"][0]["content"]["parts"][0]
                ["functionCall"]["name"],
            "read"
        );
        assert_eq!(
            encode("m", Surface::Responses, &a)["output"][0]["call_id"],
            "c"
        );
    }
    #[test]
    fn gemini_function_response_matches_prior_call_id() {
        let b = json!({"contents":[{"role":"model","parts":[{"functionCall":{"name":"read","args":{"path":"a"}}}]},{"role":"user","parts":[{"functionResponse":{"name":"read","response":{"ok":true}}}]}]});
        let r = decode(&b, Surface::Gemini, Some("m"), false, true).unwrap();
        assert_eq!(r.messages[0].calls[0].id, r.messages[1].call_id);
    }
}
