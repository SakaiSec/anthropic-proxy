use serde_json::{json, Map, Value};


pub fn openai_to_claude(mut body: Value) -> Result<Value, String> {
    let mut out = Map::new();
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .ok_or("missing model")?;
    out.insert("model".into(), json!(model));

    let Some(Value::Array(messages)) = body.get_mut("messages").map(Value::take) else {
        return Err("missing messages".into());
    };

    let mut system_texts: Vec<String> = Vec::new();
    if let Some(Value::String(s)) = body.get_mut("system").map(Value::take) {
        system_texts.push(s);
    }

    let mut claude_messages: Vec<Value> = Vec::new();
    for mut m in messages {
        let content = m.get_mut("content").map(Value::take);
        match m.get("role").and_then(Value::as_str).unwrap_or("user") {
            "system" => {
                if let Some(t) = content_text(content) {
                    system_texts.push(t);
                }
            }
            "user" => {
                let blocks = convert_content(content)?;
                if blocks.is_empty() {
                    continue;
                }
                push_msg(&mut claude_messages, "user", blocks);
            }
            "assistant" => {
                let mut blocks = convert_content(content)?;
                if let Some(Value::Array(calls)) = m.get_mut("tool_calls").map(Value::take) {
                    for c in calls {
                        let f = c.get("function");
                        let args = f
                            .and_then(|f| f.get("arguments"))
                            .and_then(Value::as_str)
                            .and_then(|s| serde_json::from_str::<Value>(s).ok())
                            .unwrap_or_else(|| json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": c.get("id").and_then(Value::as_str).unwrap_or_default(),
                            "name": mcp_name_out(f.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or_default()),
                            "input": args,
                        }));
                    }
                }
                if blocks.is_empty() {
                    continue;
                }
                push_msg(&mut claude_messages, "assistant", blocks);
            }
            "tool" => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": m.get("tool_call_id").and_then(Value::as_str).unwrap_or_default(),
                    "content": content_text(content).unwrap_or_default(),
                });
                match claude_messages.last_mut() {
                    Some(last)
                        if last.get("role").and_then(Value::as_str) == Some("user") =>
                    {
                        if let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut) {
                            arr.push(block);
                        }
                    }
                    _ => push_msg(&mut claude_messages, "user", vec![block]),
                }
            }
            _ => {}
        }
    }

    if !system_texts.is_empty() {
        out.insert("system".into(), json!(system_texts.join("\n\n")));
    }
    out.insert("messages".into(), Value::Array(cache_history(claude_messages)));
    // When absent, the server fills in the model's cap.
    if let Some(n) = body.get("max_completion_tokens").or_else(|| body.get("max_tokens")) {
        out.insert("max_tokens".into(), n.clone());
    }
    if let Some(s) = body.get("stream").and_then(Value::as_bool) {
        out.insert("stream".into(), json!(s));
    }
    if let Some(t) = convert_tools(body.get_mut("tools").map(Value::take))? {
        out.insert("tools".into(), t);
    }
    if let Some(tc) = convert_tool_choice(body.get("tool_choice")) {
        out.insert("tool_choice".into(), tc);
    }
    for k in ["temperature", "top_p", "top_k", "stop_sequences"] {
        if let Some(v) = body.get_mut(k).map(Value::take) {
            out.insert(k.to_string(), v);
        }
    }
    Ok(Value::Object(out))
}

fn push_msg(list: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if let Some(last) = list.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut) {
                arr.extend(blocks);
                return;
            }
        }
    }
    list.push(json!({ "role": role, "content": blocks }));
}

fn content_text(v: Option<Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s),
        Value::Array(items) => {
            let joined: Vec<String> = items
                .into_iter()
                .filter_map(|mut i| match i.get_mut("text").map(Value::take) {
                    Some(Value::String(t)) => Some(t),
                    _ => None,
                })
                .collect();
            if joined.is_empty() {
                None
            } else {
                Some(joined.join("\n"))
            }
        }
        _ => None,
    }
}

fn convert_content(v: Option<Value>) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    match v {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                out.push(json!({ "type": "text", "text": s }));
            }
        }
        Some(Value::Array(items)) => {
            for mut item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t @ Value::String(_)) = item.get_mut("text").map(Value::take) {
                            out.push(json!({ "type": "text", "text": t }));
                        }
                    }
                    Some("image_url") => {
                        let url = item
                            .get("image_url")
                            .and_then(|i| i.get("url"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let (media_type, data) = parse_data_url(url)
                            .ok_or_else(|| format!("unsupported image_url: {}", &url[..url.floor_char_boundary(40)]))?;
                        out.push(json!({
                            "type": "image",
                            "source": { "type": "base64", "media_type": media_type, "data": data }
                        }));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(",")?;
    let media_type = meta.split(';').next()?.to_string();
    if !meta.ends_with(";base64") {
        return None;
    }
    Some((media_type, data.to_string()))
}

pub fn mcp_name_out(name: &str) -> String {
    match name.strip_prefix("mcp_") {
        Some(rest) => match rest.chars().next() {
            Some(c) if c.is_ascii_lowercase() => format!("mcp_{}{}", c.to_ascii_uppercase(), &rest[c.len_utf8()..]),
            _ => name.to_string(),
        },
        None => name.to_string(),
    }
}

pub fn mcp_name_in(name: &str) -> String {
    match name.strip_prefix("mcp_") {
        Some(rest) => match rest.chars().next() {
            Some(c) if c.is_ascii_uppercase() => format!("mcp_{}{}", c.to_ascii_lowercase(), &rest[c.len_utf8()..]),
            _ => name.to_string(),
        },
        None => name.to_string(),
    }
}

fn convert_tools(v: Option<Value>) -> Result<Option<Value>, String> {
    let arr = match v {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(a)) => a,
        Some(_) => return Err("tools must be an array".into()),
    };
    let out = arr
        .into_iter()
        .enumerate()
        .map(|(i, mut t)| {
            let f = t
                .get_mut("function")
                .ok_or_else(|| format!("tools[{}]: missing function", i))?;
            let name = f
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .ok_or_else(|| format!("tools[{}]: missing function.name", i))?;
            let name = mcp_name_out(name);
            Ok(json!({
                "name": name,
                "description": f.get_mut("description").map(Value::take).unwrap_or(json!("")),
                "input_schema": f.get_mut("parameters").map(Value::take).unwrap_or(json!({
                    "type": "object", "properties": {}
                })),
            }))
        })
        .collect::<Result<Vec<Value>, String>>()?;
    Ok((!out.is_empty()).then_some(Value::Array(out)))
}

fn convert_tool_choice(v: Option<&Value>) -> Option<Value> {
    match v? {
        Value::String(s) => match s.as_str() {
            "auto" => None,
            "none" => Some(json!({ "type": "none" })),
            "required" => Some(json!({ "type": "any" })),
            _ => None,
        },
        Value::Object(o) => {
            let name = o.get("function")?.get("name")?.as_str()?;
            Some(json!({ "type": "tool", "name": mcp_name_out(name) }))
        }
        _ => None,
    }
}

pub fn finish_reason(stop: Option<&str>) -> &'static str {
    match stop {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        // A safety-classifier decline: HTTP 200, often with empty content.
        Some("refusal") => "content_filter",
        Some("stop_sequence") | Some("end_turn") | None => "stop",
        _ => "stop",
    }
}

pub fn claude_to_openai(claude: &Value, id: &str, created: i64) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    if let Some(blocks) = claude.get("content").and_then(Value::as_array) {
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => {
                    text.push_str(b.get("text").and_then(Value::as_str).unwrap_or_default())
                }
                Some("thinking") => {
                    reasoning.push_str(b.get("thinking").and_then(Value::as_str).unwrap_or_default())
                }
                Some("tool_use") => tool_calls.push(json!({
                    "index": tool_calls.len(),
                    "id": b.get("id").cloned().unwrap_or(json!("")),
                    "type": "function",
                    "function": {
                        "name": mcp_name_in(
                            b.get("name").and_then(Value::as_str).unwrap_or_default(),
                        ),
                        "arguments": serde_json::to_string(
                            b.get("input").unwrap_or(&Value::Null)
                        ).unwrap_or_else(|_| "{}".into()),
                    }
                })),
                _ => {}
            }
        }
    }
    let mut msg = json!({ "role": "assistant", "content": text });
    if !reasoning.is_empty() {
        msg["reasoning_content"] = json!(reasoning);
    }
    if !tool_calls.is_empty() {
        msg.as_object_mut()
            .unwrap()
            .insert("tool_calls".into(), Value::Array(tool_calls));
    }
    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": claude.get("model").cloned().unwrap_or(json!("")),
        "choices": [{
            "index": 0,
            "message": msg,
            "finish_reason": finish_reason(claude.get("stop_reason").and_then(Value::as_str)),
            "logprobs": Value::Null,
        }],
        "usage": map_usage(claude.get("usage")),
    })
}

/// On the newest turn, as Claude Code does; one turn back bills each new turn twice.
fn cache_history(mut messages: Vec<Value>) -> Vec<Value> {
    if messages.len() < 2 {
        return messages;
    }
    if let Some(block) = messages.last_mut().and_then(last_block_mut) {
        block.insert("cache_control".into(), crate::cache_breakpoint());
    }
    messages
}

fn last_block_mut(msg: &mut Value) -> Option<&mut Map<String, Value>> {
    match msg.get_mut("content")? {
        Value::Array(blocks) => match blocks.last_mut()? {
            Value::Object(map) => Some(map),
            _ => None,
        },
        _ => None,
    }
}

pub fn map_usage(u: Option<&Value>) -> Value {
    let n = |k: &str| u.and_then(|u| u.get(k)).and_then(Value::as_i64).unwrap_or(0);
    // Claude's input_tokens excludes cached input; OpenAI's prompt_tokens includes it.
    let prompt = n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens");
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": n("output_tokens"),
        "total_tokens": prompt + n("output_tokens"),
        "prompt_tokens_details": {
            "cached_tokens": n("cache_read_input_tokens"),
            "cache_write_tokens": n("cache_creation_input_tokens"),
        },
    })
}

pub fn chunk(id: &str, created: i64, model: &str, delta: Value, finish: Option<&str>, usage: Option<Value>) -> Value {
    let mut c = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "logprobs": Value::Null,
            "finish_reason": finish,
        }],
    });
    if let Some(u) = usage {
        c.as_object_mut().unwrap().insert("usage".into(), u);
    }
    c
}