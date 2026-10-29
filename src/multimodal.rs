use crate::model::Surface;
use serde_json::{json, Value};

fn valid_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

fn data_url(mime: &str, data: &str) -> Result<String, &'static str> {
    if !valid_mime(mime)
        || data.is_empty()
        || !data
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
    {
        return Err("unsupported or invalid image data");
    }
    Ok(format!("data:{mime};base64,{data}"))
}

fn image_url(url: &str, mime: Option<&str>, detail: Option<&Value>) -> Result<Value, &'static str> {
    if let Some(data) = url.strip_prefix("data:") {
        let (header, payload) = data.split_once(',').ok_or("invalid image data URL")?;
        let mime = header
            .strip_suffix(";base64")
            .ok_or("image data URL must be base64")?;
        data_url(mime, payload)?;
    } else {
        let parsed = reqwest::Url::parse(url).map_err(|_| "invalid image URL")?;
        if !matches!(parsed.scheme(), "https" | "http" | "gs") {
            return Err("unsupported image URL scheme");
        }
    }
    if mime.is_some_and(|m| !valid_mime(m)) {
        return Err("unsupported image media type");
    }
    let mut source = json!({"url":url});
    if let Some(mime) = mime {
        source["mime_type"] = json!(mime);
    }
    if let Some(detail) = detail {
        source["detail"] = detail.clone();
    }
    Ok(json!({"type":"image_url","image_url":source}))
}

pub fn from_anthropic(block: &Value) -> Result<Value, &'static str> {
    let source = &block["source"];
    match source["type"].as_str() {
        Some("base64") => {
            let mime = source["media_type"]
                .as_str()
                .ok_or("image media type required")?;
            let data = source["data"].as_str().ok_or("image data required")?;
            image_url(&data_url(mime, data)?, Some(mime), None)
        }
        Some("url") => image_url(
            source["url"].as_str().ok_or("image URL required")?,
            None,
            None,
        ),
        _ => Err("unsupported Anthropic image source"),
    }
}

pub fn from_gemini(part: &Value) -> Result<Option<Value>, &'static str> {
    if let Some(source) = part.get("inlineData").or_else(|| part.get("inline_data")) {
        let mime = source
            .get("mimeType")
            .or_else(|| source.get("mime_type"))
            .and_then(Value::as_str)
            .ok_or("image media type required")?;
        let data = source["data"].as_str().ok_or("image data required")?;
        return image_url(&data_url(mime, data)?, Some(mime), None).map(Some);
    }
    if let Some(source) = part.get("fileData").or_else(|| part.get("file_data")) {
        let mime = source
            .get("mimeType")
            .or_else(|| source.get("mime_type"))
            .and_then(Value::as_str)
            .ok_or("image media type required")?;
        let url = source
            .get("fileUri")
            .or_else(|| source.get("file_uri"))
            .and_then(Value::as_str)
            .ok_or("image URL required")?;
        return image_url(url, Some(mime), None).map(Some);
    }
    Ok(None)
}

pub fn content(content: &Value, surface: Surface) -> Result<Value, &'static str> {
    let Some(blocks) = content.as_array() else {
        return if content.is_string() || content.is_null() {
            Ok(content.clone())
        } else {
            Err("invalid message content")
        };
    };
    let mut result = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text" | "input_text" | "output_text") => {
                let text = block["text"]
                    .as_str()
                    .ok_or("text content must be a string")?;
                result.push(json!({"type":"text","text":text}));
            }
            Some("image_url" | "input_image")
                if matches!(surface, Surface::Chat | Surface::Responses) =>
            {
                let source = &block["image_url"];
                let url = source
                    .as_str()
                    .or_else(|| source["url"].as_str())
                    .ok_or("image URL required")?;
                let detail = block.get("detail").or_else(|| source.get("detail"));
                result.push(image_url(url, None, detail)?);
            }
            Some("image") if surface == Surface::Anthropic => result.push(from_anthropic(block)?),
            _ => return Err("unsupported content block"),
        }
    }
    Ok(Value::Array(result))
}

fn source(block: &Value) -> Result<(&str, Option<&str>), &'static str> {
    let image = &block["image_url"];
    let url = image["url"].as_str().ok_or("image URL required")?;
    Ok((url, image["mime_type"].as_str()))
}

fn split_data_url(url: &str) -> Option<(&str, &str)> {
    let (header, data) = url.strip_prefix("data:")?.split_once(',')?;
    Some((header.strip_suffix(";base64")?, data))
}

pub fn openai(content: &Value) -> Result<Value, &'static str> {
    let Some(blocks) = content.as_array() else {
        return Ok(content.clone());
    };
    let mut result = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => result.push(block.clone()),
            Some("image_url") => {
                let (url, _) = source(block)?;
                if url.starts_with("gs:") {
                    return Err("OpenAI upstream cannot access a gs image URI");
                }
                let mut image = json!({"url":url});
                if let Some(detail) = block["image_url"].get("detail") {
                    image["detail"] = detail.clone();
                }
                result.push(json!({"type":"image_url","image_url":image}));
            }
            _ => return Err("unsupported content block"),
        }
    }
    Ok(Value::Array(result))
}

pub fn anthropic(content: &Value) -> Result<Value, &'static str> {
    let Some(blocks) = content.as_array() else {
        return Ok(content.clone());
    };
    let mut result = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => result.push(block.clone()),
            Some("image_url") => {
                let (url, _) = source(block)?;
                let image = if let Some((mime, data)) = split_data_url(url) {
                    json!({"type":"base64","media_type":mime,"data":data})
                } else if url.starts_with("https://") || url.starts_with("http://") {
                    json!({"type":"url","url":url})
                } else {
                    return Err("Anthropic upstream cannot access this image URI");
                };
                result.push(json!({"type":"image","source":image}));
            }
            _ => return Err("unsupported content block"),
        }
    }
    Ok(Value::Array(result))
}

pub fn gemini(content: &Value) -> Result<Vec<Value>, &'static str> {
    let Some(blocks) = content.as_array() else {
        return Ok(content
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| vec![json!({"text":s})])
            .unwrap_or_default());
    };
    let mut result = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => result.push(json!({"text":block["text"]})),
            Some("image_url") => {
                let (url, hint) = source(block)?;
                if let Some((mime, data)) = split_data_url(url) {
                    result.push(json!({"inlineData":{"mimeType":mime,"data":data}}));
                } else {
                    let parsed = reqwest::Url::parse(url).map_err(|_| "invalid image URL")?;
                    let path = parsed.path().to_ascii_lowercase();
                    let mime = hint
                        .or_else(|| {
                            if path.ends_with(".png") {
                                Some("image/png")
                            } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
                                Some("image/jpeg")
                            } else if path.ends_with(".gif") {
                                Some("image/gif")
                            } else if path.ends_with(".webp") {
                                Some("image/webp")
                            } else {
                                None
                            }
                        })
                        .ok_or("Gemini image URL requires a known image media type")?;
                    result.push(json!({"fileData":{"mimeType":mime,"fileUri":url}}));
                }
            }
            _ => return Err("unsupported content block"),
        }
    }
    Ok(result)
}

pub fn has_image(content: &Value) -> bool {
    content
        .as_array()
        .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "image_url"))
}

pub fn responses(content: &Value, role: &str) -> Result<Value, &'static str> {
    let Some(blocks) = content.as_array() else {
        return Ok(content.clone());
    };
    let mut result = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => result.push(json!({"type":if role == "assistant" {"output_text"} else {"input_text"},"text":block["text"]})),
            Some("image_url") if role != "assistant" => {
                let (url, _) = source(block)?;
                if url.starts_with("gs:") { return Err("OpenAI upstream cannot access a gs image URI"); }
                let mut image = json!({"type":"input_image","image_url":url});
                if let Some(detail) = block["image_url"].get("detail") { image["detail"] = detail.clone(); }
                result.push(image);
            }
            _ => return Err("unsupported response history content"),
        }
    }
    Ok(json!(result))
}
