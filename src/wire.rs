use crate::{
    config::{Features, Upstream},
    fc,
    model::{as_text, openai_calls, parse_args, Answer, Call, Message, Request, Tool},
    multimodal,
};
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Passthrough,
    Native,
    Prompt,
}
impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Passthrough => "passthrough",
            Self::Native => "native",
            Self::Prompt => "prompt",
        }
    }
}
pub fn mode(req: &Request, up: &Upstream, features: &Features, header: Option<&str>) -> Mode {
    if req.tools.is_empty() || req.choice == "none" || req.choice["type"] == "none" {
        return Mode::Passthrough;
    }
    let mode = header.unwrap_or(&features.fc_mode).to_lowercase();
    if mode == "force_prompt" || mode == "prompt" || !up.native_fc || up.kind == "cnb" {
        Mode::Prompt
    } else {
        Mode::Native
    }
}
fn openai_message(m: &Message) -> Result<Value, &'static str> {
    let mut v = json!({"role":m.role,"content":multimodal::openai(&m.content)?});
    if !m.name.is_empty() {
        v["name"] = json!(m.name);
    }
    if !m.call_id.is_empty() {
        v["tool_call_id"] = json!(m.call_id);
    }
    if !m.calls.is_empty() {
        v["tool_calls"] = json!(openai_calls(&m.calls));
    }
    Ok(v)
}
fn tools_openai(tools: &[Tool]) -> Vec<Value> {
    tools.iter().map(|t| json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.schema}})).collect()
}
fn tools_anthropic(tools: &[Tool]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| json!({"name":t.name,"description":t.description,"input_schema":t.schema}))
        .collect()
}
fn tools_gemini(tools: &[Tool]) -> Vec<Value> {
    vec![
        json!({"functionDeclarations":tools.iter().map(|t| json!({"name":t.name,"description":t.description,"parameters":t.schema})).collect::<Vec<_>>()}),
    ]
}

pub fn build(
    req: &Request,
    up: &Upstream,
    upstream_model: &str,
    mode: Mode,
    features: &Features,
    prompt_tools: &[Tool],
) -> Result<Value, &'static str> {
    let messages = if mode == Mode::Prompt {
        fc::prompt_messages(
            &req.messages,
            prompt_tools,
            &features.inject_protocol,
            features.enable_cli_profiles,
        )
    } else {
        req.messages.clone()
    };
    for message in &req.messages {
        if multimodal::has_image(&message.content)
            && (up.kind == "cnb"
                || matches!(message.role.as_str(), "system" | "tool" | "toolResult"))
        {
            return Err("images are unsupported on this conversion path");
        }
    }
    let model = if features.model_passthrough {
        &req.model
    } else {
        upstream_model
    };
    Ok(match up.kind.as_str() {
        "anthropic" => {
            let mut v = json!({"model":model,"max_tokens":req.extra["max_tokens"].as_u64().unwrap_or(4096),"stream":req.stream});
            for key in ["temperature", "top_p"] {
                if !req.extra[key].is_null() {
                    v[key] = req.extra[key].clone();
                }
            }
            let mut system = Vec::new();
            let mut converted = Vec::new();
            let mut results = Vec::new();
            for m in &messages {
                if m.role == "system" {
                    system.push(as_text(&m.content));
                    continue;
                }
                if m.role == "tool" {
                    results.push(json!({"type":"tool_result","tool_use_id":m.call_id,"content":as_text(&m.content)}));
                    continue;
                }
                if !results.is_empty() {
                    converted.push(json!({"role":"user","content":std::mem::take(&mut results)}));
                }
                if m.role == "assistant" && !m.calls.is_empty() {
                    let content = multimodal::anthropic(&m.content)?;
                    let mut blocks = match content {
                        Value::Array(items) => items,
                        Value::String(text) if !text.is_empty() => {
                            vec![json!({"type":"text","text":text})]
                        }
                        _ => Vec::new(),
                    };
                    for c in &m.calls {
                        blocks.push(
                            json!({"type":"tool_use","id":c.id,"name":c.name,"input":c.args}),
                        );
                    }
                    converted.push(json!({"role":"assistant","content":blocks}));
                } else {
                    converted.push(json!({"role":if m.role == "assistant" {"assistant"} else {"user"},"content":multimodal::anthropic(&m.content)?}));
                }
            }
            if !results.is_empty() {
                converted.push(json!({"role":"user","content":results}));
            }
            if !system.is_empty() {
                v["system"] = json!(system.join("\n\n"));
            }
            v["messages"] = json!(converted);
            if mode == Mode::Native {
                v["tools"] = json!(tools_anthropic(&req.tools));
                v["tool_choice"] = if req.choice == "required" {
                    json!({"type":"any"})
                } else if let Some(name) = req.choice["function"]["name"].as_str() {
                    json!({"type":"tool","name":name})
                } else {
                    json!({"type":"auto"})
                };
            }
            v
        }
        "gemini" => {
            let mut v = json!({});
            let mut system = Vec::new();
            let mut contents = Vec::new();
            let mut call_names = std::collections::HashMap::new();
            for m in &messages {
                if m.role == "system" {
                    system.push(as_text(&m.content));
                    continue;
                }
                if m.role == "tool" {
                    let response = if m.content.is_object() {
                        m.content.clone()
                    } else {
                        serde_json::from_str(&as_text(&m.content))
                            .unwrap_or_else(|_| json!({"result":as_text(&m.content)}))
                    };
                    let name = if !m.name.is_empty() {
                        m.name.as_str()
                    } else {
                        call_names
                            .get(&m.call_id)
                            .map(String::as_str)
                            .unwrap_or("tool")
                    };
                    contents.push(json!({"role":"user","parts":[{"functionResponse":{"name":name,"response":response}}]}));
                    continue;
                }
                let mut parts = multimodal::gemini(&m.content)?;
                for c in &m.calls {
                    call_names.insert(c.id.clone(), c.name.clone());
                    parts.push(json!({"functionCall":{"name":c.name,"args":c.args}}));
                }
                if parts.is_empty() {
                    parts.push(json!({"text":""}));
                }
                contents.push(
                    json!({"role":if m.role == "assistant" {"model"} else {"user"},"parts":parts}),
                );
            }
            if !system.is_empty() {
                v["systemInstruction"] = json!({"parts":[{"text":system.join("\n\n")}]});
            }
            v["contents"] = json!(contents);
            let mut generation = json!({});
            for (from, to) in [
                ("temperature", "temperature"),
                ("top_p", "topP"),
                ("max_tokens", "maxOutputTokens"),
            ] {
                if !req.extra[from].is_null() {
                    generation[to] = req.extra[from].clone();
                }
            }
            if generation.as_object().is_some_and(|o| !o.is_empty()) {
                v["generationConfig"] = generation;
            }
            if mode == Mode::Native {
                v["tools"] = json!(tools_gemini(&req.tools));
                if req.choice == "required" {
                    v["toolConfig"] = json!({"functionCallingConfig":{"mode":"ANY"}});
                } else if let Some(name) = req.choice["function"]["name"].as_str() {
                    v["toolConfig"] = json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":[name]}});
                }
            }
            v
        }
        "cnb" => {
            let mut converted: Vec<Value> = Vec::new();
            for m in &messages {
                let mut role = m.role.as_str();
                let mut content = as_text(&m.content);
                if role == "tool" || role == "toolResult" {
                    role = "user";
                    content = format!("[Tool Result] {content}");
                }
                if role == "assistant" && content.is_empty() {
                    content = "(assistant called tools)".into();
                }
                if role == "user" && content.is_empty() {
                    continue;
                }
                if role == "user"
                    && converted.last().and_then(|v| v["role"].as_str()) == Some("user")
                {
                    let previous = converted.last_mut().unwrap();
                    previous["content"] = json!(format!(
                        "{}\n\n{content}",
                        previous["content"].as_str().unwrap_or("")
                    ));
                } else {
                    converted.push(json!({"role":role,"content":content}));
                }
            }
            let mut v = json!({"model":model,"stream":true,"messages":converted,"maxTokens":req.extra["max_tokens"].as_u64().unwrap_or(60000)});
            for key in ["temperature", "top_p"] {
                if !req.extra[key].is_null() {
                    v[key] = req.extra[key].clone();
                }
            }
            v
        }
        _ => {
            let mut v = req.extra.clone();
            if !v.is_object() {
                v = json!({});
            }
            v["model"] = json!(model);
            v["stream"] = json!(req.stream);
            v["messages"] = json!(messages
                .iter()
                .map(openai_message)
                .collect::<Result<Vec<_>, _>>()?);
            if mode == Mode::Native {
                v["tools"] = json!(tools_openai(&req.tools));
                v["tool_choice"] = req.choice.clone();
            }
            v
        }
    })
}
pub fn decode_result(v: &Value, kind: &str) -> Answer {
    let mut a = Answer {
        finish: "stop".into(),
        ..Default::default()
    };
    match kind {
        "anthropic" => {
            for b in v["content"].as_array().into_iter().flatten() {
                if b["type"] == "text" {
                    a.content.push_str(b["text"].as_str().unwrap_or(""));
                }
                if b["type"] == "tool_use" {
                    a.calls.push(Call {
                        id: b["id"].as_str().unwrap_or("").into(),
                        name: b["name"].as_str().unwrap_or("").into(),
                        args: parse_args(&b["input"]),
                    });
                }
            }
            if v["stop_reason"] == "max_tokens" {
                a.finish = "length".into();
            }
            a.usage = json!({"prompt_tokens":v["usage"]["input_tokens"].as_u64().unwrap_or(0),"completion_tokens":v["usage"]["output_tokens"].as_u64().unwrap_or(0)});
        }
        "gemini" => {
            for p in v["candidates"][0]["content"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if let Some(s) = p["text"].as_str() {
                    a.content.push_str(s);
                }
                if let Some(f) = p.get("functionCall").or_else(|| p.get("function_call")) {
                    a.calls.push(Call {
                        id: String::new(),
                        name: f["name"].as_str().unwrap_or("").into(),
                        args: parse_args(&f["args"]),
                    });
                }
            }
            if v["candidates"][0]["finishReason"] == "MAX_TOKENS" {
                a.finish = "length".into();
            }
            a.usage = json!({"prompt_tokens":v["usageMetadata"]["promptTokenCount"].as_u64().unwrap_or(0),"completion_tokens":v["usageMetadata"]["candidatesTokenCount"].as_u64().unwrap_or(0),"total_tokens":v["usageMetadata"]["totalTokenCount"].as_u64().unwrap_or(0)});
        }
        _ => {
            let choice = &v["choices"][0];
            let msg = &choice["message"];
            a.content = as_text(&msg["content"]);
            a.reasoning = msg["reasoning_content"].as_str().unwrap_or("").into();
            a.finish = choice["finish_reason"].as_str().unwrap_or("stop").into();
            a.usage = v["usage"].clone();
            for c in msg["tool_calls"].as_array().into_iter().flatten() {
                a.calls.push(Call {
                    id: c["id"].as_str().unwrap_or("").into(),
                    name: c["function"]["name"].as_str().unwrap_or("").into(),
                    args: parse_args(&c["function"]["arguments"]),
                });
            }
        }
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{decode, Surface};
    #[test]
    fn cnb_never_receives_native_tools() {
        let r=decode(&json!({"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}]}),Surface::Chat,None,false,true).unwrap();
        let u = Upstream {
            kind: "cnb".into(),
            native_fc: false,
            ..Default::default()
        };
        let f = Features::default();
        let body = build(&r, &u, "m", mode(&r, &u, &f, None), &f, &r.tools).unwrap();
        assert!(body.get("tools").is_none());
        assert!(body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("XYML"));
    }
}
