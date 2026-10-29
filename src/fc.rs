use crate::model::{as_text, id, Call, Message, Tool};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

const ALIASES: [(&str, &str); 9] = [
    ("Read", "fs_open_file"),
    ("Write", "fs_put_file"),
    ("Edit", "fs_patch_file"),
    ("Bash", "shell_run"),
    ("Grep", "text_search"),
    ("Glob", "path_find"),
    ("NotebookEdit", "notebook_patch"),
    ("WebFetch", "http_get_url"),
    ("WebSearch", "web_query"),
];

pub fn obfuscate(tools: &[Tool]) -> (Vec<Tool>, HashMap<String, String>) {
    let mut map = HashMap::new();
    let result = tools
        .iter()
        .enumerate()
        .map(|(i, tool)| {
            let mut safe = ALIASES
                .iter()
                .find(|(name, _)| *name == tool.name)
                .map(|(_, alias)| (*alias).to_owned())
                .unwrap_or_else(|| format!("tool_{i}"));
            if map.contains_key(&safe) {
                safe = format!("tool_{i}");
            }
            map.insert(safe.clone(), tool.name.clone());
            let mut new = tool.clone();
            new.name = safe;
            new
        })
        .collect();
    (result, map)
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}
pub fn render(name: &str, args: &Value, protocol: &str) -> String {
    let mut lines = vec![
        format!("<|{protocol}|tool_calls>"),
        format!("  <|{protocol}|invoke name=\"{}\">", escape(name)),
    ];
    if let Some(fields) = args.as_object() {
        for (key, value) in fields {
            let payload = match value.as_str() {
                Some(s) => format!("<![CDATA[{}]]>", s.replace("]]>", "]]]]><![CDATA[>")),
                None => value.to_string(),
            };
            lines.push(format!(
                "    <|{protocol}|parameter name=\"{}\">{payload}</|{protocol}|parameter>",
                escape(key)
            ));
        }
    }
    lines.push(format!("  </|{protocol}|invoke>"));
    lines.push(format!("</|{protocol}|tool_calls>"));
    lines.join("\n")
}
pub fn instructions(tools: &[Tool], protocol: &str, profiles: bool) -> String {
    let specs = tools
        .iter()
        .map(|t| {
            format!(
                "Action name: {}\nDescription: {}\nParameters: {}",
                t.name, t.description, t.schema
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let example = render("TOOL_NAME", &json!({"ARG":"value"}), protocol);
    let mut result = format!("=== {protocol} TOOL CALL PROTOCOL ===\nYou have access to these tools:\n\n{specs}\n\nFORMAT:\n{example}\n\nRULES:\n1. If a tool is needed, output a complete {protocol} tool-call block. Otherwise answer normally.\n2. Use only the listed tool and parameter names.\n3. Put strings in CDATA; use JSON for arrays and objects.\n4. Do not invent missing required parameters.\n5. After a tool result, call another tool only if needed.\n=== END {protocol} TOOL INSTRUCTIONS ===");
    if profiles {
        let names: HashSet<_> = tools
            .iter()
            .map(|tool| normalize_name(&tool.name))
            .collect();
        let contains = |name: &str| names.contains(name);
        let (profile, rules) = if contains("skillslist")
            || contains("skillview")
            || ["readfile", "terminal", "writefile"]
                .iter()
                .all(|name| contains(name))
        {
            ("Hermes", "Use the exact Hermes tool names. Prefer skills_list/skill_view when exploring skills.")
        } else if contains("sessionsspawn") || contains("exec") {
            ("OpenClaw", "Use available read/write/exec tools for files; use process for background job control.")
        } else if ["bash", "read", "write"].iter().all(|name| contains(name))
            || contains("task")
            || contains("agent")
        {
            (
                "Claude Code",
                "Use exact listed tool names. Prefer direct tools when one step suffices.",
            )
        } else if ["bash", "read", "write", "edit"]
            .iter()
            .any(|name| contains(name))
            && contains("todowrite")
        {
            (
                "OpenCode",
                "Use exact listed lowercase tool names. todowrite is for planning only.",
            )
        } else {
            ("Generic CLI", "Use the exact listed tool names.")
        };
        result.push_str(&format!("\n\n[CLIENT TOOL PROFILE: {profile}]\n{rules}"));
    }
    result
}
pub fn prompt_messages(
    messages: &[Message],
    tools: &[Tool],
    protocol: &str,
    profiles: bool,
) -> Vec<Message> {
    let mut out = Vec::new();
    for m in messages {
        if m.role == "tool" || m.role == "toolResult" {
            out.push(Message {
                role: "user".into(),
                content: json!(format!(
                    "[Tool Result id={} name={}]\n{}",
                    m.call_id,
                    m.name,
                    as_text(&m.content)
                )),
                ..Default::default()
            });
        } else if m.role == "assistant" && !m.calls.is_empty() {
            let calls = m
                .calls
                .iter()
                .map(|c| render(&c.name, &c.args, protocol))
                .collect::<Vec<_>>()
                .join("\n");
            let content = if let Some(blocks) = m.content.as_array() {
                let mut blocks = blocks.clone();
                blocks.push(json!({"type":"text","text":calls}));
                json!(blocks)
            } else {
                json!(format!("{}\n{calls}", as_text(&m.content)))
            };
            out.push(Message {
                role: "assistant".into(),
                content,
                ..Default::default()
            });
        } else {
            out.push(Message {
                role: if matches!(m.role.as_str(), "system" | "user" | "assistant") {
                    m.role.clone()
                } else {
                    "user".into()
                },
                content: m.content.clone(),
                ..Default::default()
            });
        }
    }
    let instruction = instructions(tools, protocol, profiles);
    if let Some(first) = out.first_mut().filter(|m| m.role == "system") {
        first.content = json!(format!("{}\n\n{instruction}", as_text(&first.content)));
    } else {
        out.insert(
            0,
            Message {
                role: "system".into(),
                content: json!(instruction),
                ..Default::default()
            },
        );
    }
    out
}
fn normalize_name(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}
fn allowed_name<'a>(name: &str, tools: &'a [Tool]) -> Option<&'a Tool> {
    let norm = normalize_name(name.strip_prefix("u_").unwrap_or(name));
    tools.iter().find(|t| {
        normalize_name(&t.name) == norm
            || ALIASES
                .iter()
                .any(|(real, safe)| t.name == *real && normalize_name(safe) == norm)
    })
}
fn attr(s: &str) -> String {
    let re = Regex::new(r#"(?i)\bname\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s/>]+))"#).unwrap();
    re.captures(s)
        .and_then(|c| c.get(1).or_else(|| c.get(2)).or_else(|| c.get(3)))
        .map(|m| unescape(m.as_str()))
        .unwrap_or_default()
}
fn scalar(raw: &str, name: &str) -> Value {
    let cdata = Regex::new(r"(?is)<!\[CDATA\[(.*?)\]\]>").unwrap();
    let chunks: Vec<_> = cdata.captures_iter(raw).map(|c| c[1].to_owned()).collect();
    let s = if chunks.is_empty() {
        unescape(raw.trim())
    } else {
        chunks.join("")
    };
    if matches!(
        name.to_lowercase().as_str(),
        "content"
            | "command"
            | "cmd"
            | "script"
            | "code"
            | "prompt"
            | "file_content"
            | "old_string"
            | "new_string"
            | "insert_text"
            | "patch"
            | "pattern"
            | "text"
            | "query"
            | "url"
            | "path"
            | "file_path"
    ) {
        return json!(s);
    }
    if chunks.is_empty() && raw.contains('<') {
        if let Some(value) = nested_markup(raw) {
            return value;
        }
    }
    serde_json::from_str(&s).unwrap_or_else(|_| json!(s))
}
fn nested_markup(raw: &str) -> Option<Value> {
    fn children(node: roxmltree::Node<'_, '_>) -> Value {
        let nodes: Vec<_> = node.children().filter(|n| n.is_element()).collect();
        if nodes.is_empty() {
            let text = node.children().filter_map(|n| n.text()).collect::<String>();
            return scalar(&text, node.tag_name().name());
        }
        if nodes
            .iter()
            .all(|n| n.tag_name().name().eq_ignore_ascii_case("item"))
        {
            return Value::Array(nodes.into_iter().map(children).collect());
        }
        let mut out = Map::new();
        for item in nodes {
            let name = item.tag_name().name().to_owned();
            let value = children(item);
            if let Some(existing) = out.get_mut(&name) {
                if let Some(values) = existing.as_array_mut() {
                    values.push(value);
                } else {
                    *existing = json!([existing.clone(), value]);
                }
            } else {
                out.insert(name, value);
            }
        }
        Value::Object(out)
    }
    let wrapped = format!("<root>{raw}</root>");
    let document = roxmltree::Document::parse(&wrapped).ok()?;
    if !document.root_element().children().any(|n| n.is_element()) {
        return None;
    }
    Some(children(document.root_element()))
}
fn key_values(raw: &str) -> Value {
    let mut result = Map::new();
    for line in raw.lines() {
        let pair = line.split_once('=').or_else(|| line.split_once(':'));
        if let Some((k, v)) = pair {
            let key = k.trim();
            if !key.is_empty() {
                result.insert(key.into(), scalar(v.trim().trim_matches(['\"', '\'']), key));
            }
        }
    }
    Value::Object(result)
}
fn schema_has_type(schema: &Value, ty: &str, depth: usize) -> bool {
    if depth > 32 {
        return false;
    }
    schema["type"] == ty
        || schema["type"]
            .as_array()
            .is_some_and(|types| types.iter().any(|item| item == ty))
        || (ty == "object" && schema["properties"].is_object())
        || (ty == "array" && schema["items"].is_object())
        || ["anyOf", "oneOf", "allOf"].iter().any(|key| {
            schema[*key].as_array().is_some_and(|variants| {
                variants
                    .iter()
                    .any(|variant| schema_has_type(variant, ty, depth + 1))
            })
        })
}

fn coerce(value: &mut Value, schema: &Value) {
    let array = schema_has_type(schema, "array", 0);
    let object = schema_has_type(schema, "object", 0);
    if array || object {
        if let Some(s) = value.as_str() {
            let parsed = json5::from_str::<Value>(s).or_else(|_| {
                if array && !s.trim_start().starts_with('[') {
                    json5::from_str::<Value>(&format!("[{s}]"))
                } else {
                    json5::from_str::<Value>("")
                }
            });
            if let Ok(v) = parsed {
                *value = v;
            }
        }
        if array && value.is_object() {
            *value = json!([value.clone()]);
        }
    }
    if let Some(obj) = value.as_object_mut() {
        for (k, v) in obj {
            coerce(v, &schema["properties"][k]);
        }
    }
    if let Some(arr) = value.as_array_mut() {
        for v in arr {
            coerce(v, &schema["items"]);
        }
    }
}
fn accept(tool: &Tool, mut args: Value, call_id: Option<&str>, out: &mut Vec<Call>) {
    if !args.is_object() {
        args = json!({"value":args});
    }
    fn rename(fields: &mut Map<String, Value>, key: &str, aliases: &[&str]) {
        if fields.contains_key(key) {
            return;
        }
        if let Some(alias) = aliases.iter().find(|alias| fields.contains_key(**alias)) {
            if let Some(value) = fields.remove(*alias) {
                fields.insert(key.to_owned(), value);
            }
        }
    }
    let fields = args.as_object_mut().unwrap();
    match tool.name.as_str() {
        "Read" => rename(fields, "file_path", &["path", "filename", "file"]),
        "Write" => {
            rename(
                fields,
                "file_path",
                &["path", "target_file", "filename", "file"],
            );
            rename(
                fields,
                "content",
                &["text", "body", "data", "file_content", "contents", "value"],
            );
        }
        "Edit" => rename(
            fields,
            "file_path",
            &["path", "target_file", "filename", "file"],
        ),
        "Bash" | "PowerShell" => rename(fields, "command", &["cmd", "script"]),
        "Agent" => {
            let description = fields
                .entry("description")
                .or_insert_with(|| json!("Execute sub-task"))
                .clone();
            fields.entry("prompt").or_insert(description);
        }
        "AskUserQuestion" => {
            if !fields.contains_key("questions") {
                if let Some(question) = fields.remove("question") {
                    fields.insert("questions".into(),json!([{"question":question,"header":"Question","multiSelect":false,"options":[{"label":"Yes","description":"Confirm"},{"label":"No","description":"Decline"}]}]));
                }
            }
            if let Some(value) = fields.get_mut("questions") {
                if !value.is_array() {
                    *value = json!([value.clone()]);
                }
            }
        }
        _ if !fields.contains_key("query") && tool.schema["properties"]["query"].is_object() => {
            if let Some(queries) = fields.remove("queries") {
                let query = if let Some(items) = queries.as_array() {
                    items.iter().map(as_text).collect::<Vec<_>>().join("\n")
                } else {
                    as_text(&queries)
                };
                fields.insert("query".into(), json!(query));
            }
        }
        _ => (),
    }
    coerce(&mut args, &tool.schema);
    let mut required: HashSet<&str> = tool.schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    match tool.name.as_str() {
        "Read" | "Edit" => {
            required.insert("file_path");
        }
        "Write" => {
            required.insert("file_path");
            required.insert("content");
        }
        "Bash" | "PowerShell" => {
            required.insert("command");
        }
        _ => (),
    }
    for key in required {
        let v = &args[key];
        let allow_empty = tool.name == "Write" && key == "content";
        if v.is_null() || (!allow_empty && v.as_str().is_some_and(|s| s.trim().is_empty())) {
            return;
        }
    }
    if let Some(fields) = args.as_object() {
        for (key, v) in fields {
            if matches!(
                normalize_name(key).as_str(),
                "path"
                    | "filepath"
                    | "filename"
                    | "targetfile"
                    | "file"
                    | "dir"
                    | "directory"
                    | "cwd"
                    | "workdir"
                    | "workingdirectory"
            ) {
                if let Some(s) = v.as_str() {
                    let lower = s.to_lowercase();
                    if s.is_empty()
                        || s.contains(['\0', '\r', '\n', '<', '>'])
                        || [
                            "xyml|",
                            "qnml|",
                            "tool_calls",
                            "invoke name=",
                            "parameter name=",
                        ]
                        .iter()
                        .any(|m| lower.contains(m))
                    {
                        return;
                    }
                }
            }
        }
    }
    out.push(Call {
        id: call_id
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| id("call_")),
        name: tool.name.clone(),
        args,
    });
}
fn json_calls(v: &Value, tools: &[Tool], out: &mut Vec<Call>) {
    if let Some(items) = v.as_array() {
        for item in items {
            json_calls(item, tools, out);
        }
        return;
    }
    if !v.is_object() {
        return;
    }
    for key in ["tool_calls", "tools"] {
        if let Some(items) = v[key].as_array() {
            for item in items {
                json_calls(item, tools, out);
            }
        }
    }
    let function = &v["function"];
    let name = ["name", "tool", "tool_name", "function_name"]
        .iter()
        .find_map(|k| v[*k].as_str())
        .or_else(|| function["name"].as_str());
    if let Some(tool) = name.and_then(|n| allowed_name(n, tools)) {
        let a = ["input", "arguments", "args", "parameters"]
            .iter()
            .find_map(|k| v.get(*k))
            .or_else(|| {
                ["arguments", "input", "parameters"]
                    .iter()
                    .find_map(|key| function.get(*key))
            })
            .cloned()
            .unwrap_or_else(|| json!({}));
        let args = if let Some(s) = a.as_str() {
            json5::from_str(s).unwrap_or_else(|_| {
                let fields = key_values(s);
                if fields.as_object().is_some_and(|values| !values.is_empty()) {
                    fields
                } else {
                    json!({"value":s})
                }
            })
        } else {
            a
        };
        accept(
            tool,
            args,
            v["id"].as_str().or_else(|| v["call_id"].as_str()),
            out,
        );
    }
}

fn recover_loose_json(text: &str) -> Option<Value> {
    if text.len() > 256 * 1024 {
        return None;
    }
    let name_open = Regex::new(r#"(?i)"name="\s*"#).unwrap();
    let name_value = Regex::new(r#"(?i)"name=([^",}\s]+)""#).unwrap();
    let equals = Regex::new(
        r#"(?i)"(name|input|arguments|args|parameters|tool|tool_name|function_name)"\s*=\s*"#,
    )
    .unwrap();
    let repaired = name_open.replace_all(text, "\"name\": \"");
    let repaired = name_value.replace_all(&repaired, "\"name\": \"$1\"");
    let repaired = equals.replace_all(&repaired, "\"$1\": ");
    if let Ok(value) = json5::from_str(&repaired) {
        return Some(value);
    }
    let mut stack = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    for byte in repaired.bytes() {
        if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == delimiter {
                quote = None;
            }
        } else {
            match byte {
                b'\'' | b'"' => quote = Some(byte),
                b'{' => stack.push('}'),
                b'[' => stack.push(']'),
                b'}' | b']' if stack.pop() != Some(byte as char) => return None,
                _ => (),
            }
        }
        if stack.len() > 8 {
            return None;
        }
    }
    if quote.is_some() || stack.is_empty() {
        return None;
    }
    let suffix: String = stack.into_iter().rev().collect();
    json5::from_str(&format!("{repaired}{suffix}")).ok()
}

fn loose_protocol_calls(text: &str, protocol: &str, tools: &[Tool], out: &mut Vec<Call>) {
    let marker = Regex::new(&format!(r"(?i)\b{}\b", regex::escape(protocol))).unwrap();
    if !marker.is_match(text) {
        return;
    }
    let attrs =
        Regex::new(r#"(?is)\b(?:name|parameter)\s*=\s*(?:"([^"]*)"|'([^']*)'|([A-Za-z0-9_.:-]+))"#)
            .unwrap();
    let cdata = Regex::new(r"(?is)<!\[CDATA\[(.*?)\]\]>").unwrap();
    let attributes: Vec<_> = attrs
        .captures_iter(text)
        .take(256)
        .filter_map(|capture| {
            let found = capture.get(0)?;
            let raw = capture
                .get(1)
                .or_else(|| capture.get(2))
                .or_else(|| capture.get(3))?
                .as_str();
            Some((raw, found.start(), allowed_name(raw, tools)))
        })
        .collect();
    for (index, (_, _, tool)) in attributes.iter().enumerate() {
        let Some(tool) = tool else {
            continue;
        };
        let end = attributes[index + 1..]
            .iter()
            .find_map(|(_, position, next)| next.is_some().then_some(*position))
            .unwrap_or(text.len());
        let mut fields = Map::new();
        for (name, position, next) in &attributes[index + 1..] {
            if *position >= end || next.is_some() {
                break;
            }
            if let Some(value) = cdata
                .captures(&text[*position..end])
                .and_then(|cap| cap.get(1))
            {
                let properties = tool.schema["properties"].as_object();
                if properties.is_none_or(|properties| {
                    properties.is_empty() || properties.contains_key(*name)
                }) {
                    fields.insert((*name).to_owned(), scalar(value.as_str(), name));
                }
            }
        }
        accept(tool, Value::Object(fields), None, out);
    }
}

pub fn parse(source: &str, tools: &[Tool], protocol: &str, strip_think: bool) -> Vec<Call> {
    if tools.is_empty() {
        return Vec::new();
    }
    let think = Regex::new(r"(?is)<think>.*?</think>").unwrap();
    let text = if strip_think {
        think.replace_all(source, "").into_owned()
    } else {
        source.to_owned()
    };
    let normalized = text
        .replace(['\u{200b}', '\u{200c}', '\u{200d}', '\u{feff}'], "")
        .replace(['\u{3000}', '\u{00a0}'], " ")
        .replace(['＜', '﹤'], "<")
        .replace(['＞', '﹥'], ">")
        .replace(['／', '∕', '⁄'], "/")
        .replace(['｜', '│', '┃', '▏', '▕'], "|")
        .replace('＝', "=")
        .replace(['“', '”'], "\"")
        .replace(['‘', '’'], "'");
    let mut calls = Vec::new();
    let mut protocols = vec![protocol, "XYML", "QNML"];
    protocols.dedup();
    for p in protocols {
        let before = calls.len();
        let invoke = Regex::new(&format!(
            r"(?is)<\s*\|\s*{}\s*\|\s*invoke\b([^>]*)>(.*?)<\s*/\s*\|\s*{}\s*\|\s*invoke\s*>",
            regex::escape(p),
            regex::escape(p)
        ))
        .unwrap();
        let param = Regex::new(&format!(
            r"(?is)<\s*\|\s*{}\s*\|\s*parameter\b([^>]*)>(.*?)<\s*/\s*\|\s*{}\s*\|\s*parameter\s*>",
            regex::escape(p),
            regex::escape(p)
        ))
        .unwrap();
        for cap in invoke.captures_iter(&normalized) {
            if let Some(tool) = allowed_name(&attr(&cap[1]), tools) {
                let mut args = Map::new();
                for c in param.captures_iter(&cap[2]) {
                    let name = attr(&c[1]);
                    if !name.is_empty() {
                        args.insert(name.clone(), scalar(&c[2], &name));
                    }
                }
                if args.is_empty() {
                    if let Value::Object(fields) = key_values(&cap[2]) {
                        args = fields;
                    }
                }
                accept(tool, Value::Object(args), None, &mut calls);
            }
        }
        if calls.len() == before {
            loose_protocol_calls(&normalized, p, tools, &mut calls);
        }
    }
    let xml = Regex::new(r"(?is)<(?:tool_use|tool_call|function|invoke)\b([^>]*)>(.*?)</(?:tool_use|tool_call|function|invoke)\s*>").unwrap();
    for cap in xml.captures_iter(&normalized) {
        if let Some(tool) = allowed_name(&attr(&cap[1]), tools) {
            let content = cap[2].trim();
            let args = serde_json::from_str::<Value>(content)
                .or_else(|_| json5::from_str(content))
                .unwrap_or_else(|_| nested_markup(content).unwrap_or_else(|| key_values(content)));
            accept(tool, args, None, &mut calls);
        } else if let Ok(v) = serde_json::from_str::<Value>(cap[2].trim()) {
            json_calls(&v, tools, &mut calls);
        }
    }
    let trimmed = normalized
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let Some(v) = json5::from_str::<Value>(trimmed)
        .ok()
        .or_else(|| recover_loose_json(trimmed))
    {
        json_calls(&v, tools, &mut calls);
    }
    let mut skip_until = 0;
    let mut attempts = 0;
    for (i, _) in normalized
        .char_indices()
        .filter(|(_, c)| *c == '{' || *c == '[')
    {
        if i < skip_until {
            continue;
        }
        if attempts >= 128 || i > 1024 * 1024 {
            break;
        }
        attempts += 1;
        let mut end = (i + 256 * 1024).min(normalized.len());
        while !normalized.is_char_boundary(end) {
            end -= 1;
        }
        let mut it = serde_json::Deserializer::from_str(&normalized[i..end]).into_iter::<Value>();
        if let Some(Ok(v)) = it.next() {
            skip_until = i + it.byte_offset();
            json_calls(&v, tools, &mut calls);
        }
    }
    let mut name = None;
    let mut arguments = String::new();
    let mut collecting = false;
    for line in normalized.lines() {
        if let Some((key, value)) = line.split_once(':') {
            match key.trim().to_ascii_lowercase().as_str() {
                "function.name" | "name" | "tool" | "tool.name" | "tool_name" => {
                    name = Some(value.trim().trim_matches(['\"', '\'']).to_owned());
                    collecting = false;
                }
                "function.arguments" | "arguments" | "args" | "input" | "tool_input"
                | "parameters" => {
                    arguments.push_str(value.trim());
                    collecting = true;
                }
                _ if collecting => {
                    arguments.push('\n');
                    arguments.push_str(line);
                }
                _ => (),
            }
        } else if collecting {
            arguments.push('\n');
            arguments.push_str(line);
        }
    }
    if let Some(tool) = name.as_deref().and_then(|n| allowed_name(n, tools)) {
        let args = serde_json::from_str(&arguments)
            .or_else(|_| json5::from_str(&arguments))
            .unwrap_or_else(|_| key_values(&arguments));
        accept(tool, args, None, &mut calls);
    }
    let mut seen = HashSet::new();
    calls.retain(|c| seen.insert(format!("{}\0{}", c.name, c.args)));
    calls
}
pub fn looks_like_failed_call(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "|tool_calls>",
        "<tool_calls>",
        "<tool_use>",
        "\"tool_calls\":",
        "function.name:",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tools() -> Vec<Tool> {
        vec![Tool {
            name: "get_weather".into(),
            description: "".into(),
            schema: json!({"type":"object","properties":{"city":{"type":"string"},"units":{"type":"string"}},"required":["city"]}),
        }]
    }
    #[test]
    fn toolforge_reference_corpus() {
        let corpus: Value =
            serde_json::from_str(include_str!("../tests/parser_corpus.json")).unwrap();
        let tools: Vec<Tool> = corpus["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| Tool {
                name: value["function"]["name"].as_str().unwrap().into(),
                description: String::new(),
                schema: value["function"]["parameters"].clone(),
            })
            .collect();
        let generated: Value =
            serde_json::from_str(include_str!("../tests/parser_generated.json")).unwrap();
        for case in corpus["cases"]
            .as_array()
            .unwrap()
            .iter()
            .chain(generated.as_array().unwrap())
        {
            let calls = parse(case["text"].as_str().unwrap(), &tools, "XYML", true)
                .iter()
                .map(|call| json!({"name":call.name,"arguments":call.args}))
                .collect::<Vec<_>>();
            assert_eq!(json!(calls), case["calls"], "{}", case["name"]);
        }
    }
    #[test]
    fn markup_roundtrip_and_required_validation() {
        let block = render("get_weather", &json!({"city":"Tokyo"}), "XYML");
        assert_eq!(
            parse(&block, &tools(), "XYML", true)[0].args["city"],
            "Tokyo"
        );
        assert!(parse(
            &render("get_weather", &json!({}), "XYML"),
            &tools(),
            "XYML",
            true
        )
        .is_empty());
        assert!(parse(
            &render("other", &json!({"city":"x"}), "XYML"),
            &tools(),
            "XYML",
            true
        )
        .is_empty());
    }
    #[test]
    fn json_and_qnml_are_accepted() {
        assert_eq!(
            parse(
                "{name: \"get_weather\", arguments: {city: \"Paris\"}}",
                &tools(),
                "XYML",
                true
            )[0]
            .args["city"],
            "Paris"
        );
        assert_eq!(
            parse(
                r#"text {"tool_calls":[{"name":"get_weather","arguments":{"city":"Paris"}}]}"#,
                &tools(),
                "XYML",
                true
            )[0]
            .args["city"],
            "Paris"
        );
        assert_eq!(
            parse(
                &render("get_weather", &json!({"city":"Seoul"}), "QNML"),
                &tools(),
                "XYML",
                true
            )[0]
            .args["city"],
            "Seoul"
        );
    }
    #[test]
    fn nested_arrays_and_loose_json_are_supported() {
        let tools = vec![Tool {
            name: "search".into(),
            description: String::new(),
            schema: json!({"type":"object","properties":{"filters":{"type":"array","items":{"type":"object"}}},"required":["filters"]}),
        }];
        let text="<|XYML|invoke name=\"search\"><|XYML|parameter name=\"filters\"><item><key>one</key></item><item><key>two</key></item></|XYML|parameter></|XYML|invoke>";
        assert_eq!(
            parse(text, &tools, "XYML", true)[0].args["filters"][1]["key"],
            "two"
        );
        assert_eq!(
            parse(
                "{'name':'search','arguments':{'filters':[{'key':'v'}]}}",
                &tools,
                "XYML",
                true
            )[0]
            .args["filters"][0]["key"],
            "v"
        );
    }
    #[test]
    fn cli_aliases_and_profiles_are_applied() {
        let tools = vec![Tool {
            name: "Read".into(),
            description: String::new(),
            schema: json!({"type":"object","properties":{"file_path":{"type":"string"}},"required":["file_path"]}),
        }];
        let block = render("fs_open_file", &json!({"path":"src/main.rs"}), "XYML");
        let calls = parse(&block, &tools, "XYML", true);
        assert_eq!(calls[0].name, "Read");
        assert_eq!(calls[0].args["file_path"], "src/main.rs");
        let bash = Tool {
            name: "Bash".into(),
            description: String::new(),
            schema: json!({}),
        };
        let write = Tool {
            name: "Write".into(),
            description: String::new(),
            schema: json!({}),
        };
        assert!(instructions(&[tools[0].clone(), bash, write], "XYML", true)
            .contains("CLIENT TOOL PROFILE: Claude Code"));
    }
}
