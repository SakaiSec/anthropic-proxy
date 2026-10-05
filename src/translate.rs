use serde_json::{json, Map, Value};

pub fn openai_to_claude(body: &Value) -> Result<Value, String> {
    let mut out = Map::new();
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .ok_or("missing model")?;
    out.insert("model".into(), json!(model));

    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("missing messages")?;

    let mut system_texts: Vec<String> = Vec::new();
    if let Some(s) = body.get("system").and_then(Value::as_str) {
        system_texts.push(s.to_string());
    }

    let mut claude_messages: Vec<Value> = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        match role {
            "system" => {
                if let Some(t) = content_text(m.get("content")) {
                    system_texts.push(t);
                }
            }
            "user" => {
                let blocks = convert_content(m.get("content"))?;
                if blocks.is_empty() {
                    continue;
                }
                push_msg(&mut claude_messages, "user", blocks);
            }
            "assistant" => {
                let mut blocks = convert_content(m.get("content"))?;
                if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                    for c in calls {
                        let f = c.get("function").cloned().unwrap_or(Value::Null);
                        let args = f
                            .get("arguments")
                            .and_then(Value::as_str)
                            .and_then(|s| serde_json::from_str::<Value>(s).ok())
                            .unwrap_or_else(|| json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": c.get("id").and_then(Value::as_str).unwrap_or_default(),
                            "name": mcp_name_out(f.get("name").and_then(Value::as_str).unwrap_or_default()),
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
                    "content": content_text(m.get("content")).unwrap_or_default(),
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
    out.insert(
        "max_tokens".into(),
        body.get("max_tokens").cloned().unwrap_or(json!(4096)),
    );
    if let Some(s) = body.get("stream").and_then(Value::as_bool) {
        out.insert("stream".into(), json!(s));
    }
    if let Some(t) = convert_tools(body.get("tools"))? {
        out.insert("tools".into(), t);
    }
    if let Some(tc) = convert_tool_choice(body.get("tool_choice")) {
        out.insert("tool_choice".into(), tc);
    }
    for k in ["temperature", "top_p", "top_k", "stop_sequences"] {
        if let Some(v) = body.get(k) {
            out.insert(k.to_string(), v.clone());
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

fn content_text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            let joined: Vec<String> = items
                .iter()
                .filter_map(|i| i.get("text").and_then(Value::as_str).map(str::to_string))
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

fn convert_content(v: Option<&Value>) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    match v {
        None => {}
        Some(Value::String(s)) => {
            if !s.is_empty() {
                out.push(json!({ "type": "text", "text": s }));
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = item.get("text").and_then(Value::as_str) {
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

fn convert_tools(v: Option<&Value>) -> Result<Option<Value>, String> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let arr = v.as_array().ok_or("tools must be an array")?;
    let out = arr
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let f = t
                .get("function")
                .ok_or_else(|| format!("tools[{}]: missing function", i))?;
            let name = f
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .ok_or_else(|| format!("tools[{}]: missing function.name", i))?;
            Ok(json!({
                "name": mcp_name_out(name),
                "description": f.get("description").cloned().unwrap_or(json!("")),
                "input_schema": f.get("parameters").cloned().unwrap_or(json!({
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
        Some("stop_sequence") | Some("end_turn") | None => "stop",
        _ => "stop",
    }
}

pub fn claude_to_openai(claude: &Value, id: &str, created: i64) -> Value {
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    if let Some(blocks) = claude.get("content").and_then(Value::as_array) {
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => {
                    text.push_str(b.get("text").and_then(Value::as_str).unwrap_or_default())
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
    if !tool_calls.is_empty() {
        msg.as_object_mut()
            .unwrap()
            .insert("tool_calls".into(), Value::Array(tool_calls.clone()));
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

fn cache_history(mut messages: Vec<Value>) -> Vec<Value> {
    if messages.len() < 2 {
        return messages;
    }
    let newest = messages.pop().unwrap();
    if let Some(block) = last_block_mut(messages.last_mut().unwrap()) {
        block.insert("cache_control".into(), crate::cache_breakpoint());
    }
    messages.push(newest);
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
    json!({
        "prompt_tokens": n("input_tokens"),
        "completion_tokens": n("output_tokens"),
        "total_tokens": n("input_tokens") + n("output_tokens"),
        "prompt_tokens_details": { "cached_tokens": n("cache_read_input_tokens") },
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