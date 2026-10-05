use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::translate;
use crate::{cloaked_system, http_client, load_token, refresh_token, Ctx};

const BETAS: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
context-management-2025-06-27,prompt-caching-scope-2026-01-05,extended-cache-ttl-2025-04-11";

pub async fn chat_completions(
    State(ctx): State<Ctx>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let parsed: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return err(400, json!({ "error": { "message": e.to_string(), "type": "invalid_request_error" } })),
    };
    let claude = match translate::openai_to_claude(&parsed) {
        Ok(v) => v,
        Err(e) => return err(400, json!({ "error": { "message": e, "type": "invalid_request_error" } })),
    };
    let streaming = parsed.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let model = claude.get("model").and_then(Value::as_str).unwrap_or_default().to_string();
    let id = format!("chatcmpl-{}", crate::rand_id());
    let created = now();

    let session = session_id(&headers);
    let payload = cloaked_system(claude, &session, &ctx.creds);

    let upstream = match send(&ctx, &payload, &session, streaming).await {
        Ok(r) => r,
        Err(e) => return err(502, json!({ "error": { "message": e, "type": "upstream_error" } })),
    };

    let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        let body = match upstream.bytes().await {
            Ok(b) if !b.is_empty() => b,
            Ok(_) => upstream_error_body(format!("upstream returned {} with an empty body", status)),
            Err(e) => upstream_error_body(format!("upstream returned {}; reading its body failed: {}", status, e)),
        };
        return (status, [(axum::http::header::CONTENT_TYPE, "application/json")], body).into_response();
    }

    if streaming {
        stream_response(upstream, id, created, model)
    } else {
        let bytes = match upstream.bytes().await {
            Ok(b) => b,
            Err(e) => return upstream_failure(format!("reading upstream body failed: {}", e)),
        };
        let claude: Value = match serde_json::from_slice(&bytes) {
            Ok(v @ Value::Object(_)) => v,
            Ok(_) => return upstream_failure("upstream body is not a JSON object".into()),
            Err(e) => return upstream_failure(format!("upstream body is not valid JSON ({} bytes): {}", bytes.len(), e)),
        };
        json_ok(translate::claude_to_openai(&claude, &id, created))
    }
}

fn upstream_error_body(message: String) -> axum::body::Bytes {
    serde_json::to_vec(&json!({ "error": { "message": message, "type": "upstream_error" } }))
        .unwrap_or_default()
        .into()
}

fn upstream_failure(message: String) -> Response {
    eprintln!("{}", message);
    err(502, json!({ "error": { "message": message, "type": "upstream_error" } }))
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn session_id(headers: &HeaderMap) -> String {
    headers
        .get("x-claude-code-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(crate::rand_id)
}

async fn send(
    ctx: &Ctx,
    payload: &Value,
    session: &str,
    streaming: bool,
) -> Result<wreq::Response, String> {
    let url = format!("{}/v1/messages", ctx.upstream);
    let mut token = load_token(&ctx.creds)?;
    if token.is_expiring() {
        token = refresh_token(ctx, None)
            .await
            .map_err(|e| format!("token expired and refresh failed: {}", e))?;
    }
    for attempt in 0..2 {
        let r = http_client()
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", if streaming { "text/event-stream" } else { "application/json" })
            .header("authorization", format!("Bearer {}", token.access_token))
            .header("anthropic-beta", BETAS)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-dangerous-direct-browser-access", "true")
            .header("user-agent", crate::CC_USER_AGENT)
            .header("x-app", "cli")
            .header("x-stainless-lang", "js")
            .header("x-stainless-package-version", crate::SDK_VERSION)
            .header("x-stainless-runtime", "node")
            .header("x-stainless-runtime-version", crate::NODE_VERSION)
            .header("x-stainless-os", "MacOS")
            .header("x-stainless-arch", "arm64")
            .header("x-stainless-retry-count", "0")
            .header("x-stainless-timeout", "600")
            .header("x-claude-code-session-id", session)
            .json(payload)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if r.status().as_u16() != 401 || attempt == 1 {
            return Ok(r);
        }
        drop(r);
        token = refresh_token(ctx, Some(&token.access_token))
            .await
            .map_err(|e| format!("401 and refresh failed: {}", e))?;
    }
    unreachable!()
}

pub struct SseState {
    pub id: String,
    pub created: i64,
    pub model: String,
    pub buf: String,
    pub usage: Option<Value>,
    pub tool_count: usize,
    pub tool_by_block: HashMap<usize, usize>,
    pub stop: Option<String>,
    pub done: bool,
    pub finished: bool,
}

pub fn handle_event(st: &mut SseState, raw: &str, out: &mut String) {
    let Ok(ev) = serde_json::from_str::<Value>(raw) else { return };
    match ev.get("type").and_then(Value::as_str) {
        Some("message_start") => {
            st.usage = ev.pointer("/message/usage").cloned();
            out.push_str(&sse(&translate::chunk(
                &st.id, st.created, &st.model, json!({ "role": "assistant", "content": "" }), None, None,
            )));
        }
        Some("content_block_start") => {
            let idx = ev.get("index").and_then(Value::as_i64).unwrap_or(0) as usize;
            let block = ev.get("content_block").cloned().unwrap_or(Value::Null);
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let t = block.get("text").and_then(Value::as_str).unwrap_or("");
                    if !t.is_empty() {
                        out.push_str(&sse(&translate::chunk(
                            &st.id, st.created, &st.model, json!({ "content": t }), None, None,
                        )));
                    }
                }
                Some("tool_use") => {
                    st.tool_count += 1;
                    st.tool_by_block.insert(idx, st.tool_count - 1);
                    out.push_str(&sse(&translate::chunk(
                        &st.id, st.created, &st.model,
                        json!({ "tool_calls": [{
                            "index": st.tool_count - 1,
                            "id": block.get("id").cloned().unwrap_or(json!("")),
                            "type": "function",
                            "function": {
                                "name": translate::mcp_name_in(
                                    block.get("name").and_then(Value::as_str).unwrap_or_default(),
                                ),
                                "arguments": ""
                            }
                        }]}),
                        None, None,
                    )));
                }
                _ => {}
            }
        }
        Some("content_block_delta") => {
            let idx = ev.get("index").and_then(Value::as_i64).unwrap_or(0) as usize;
            let d = ev.get("delta").cloned().unwrap_or(Value::Null);
            match d.get("type").and_then(Value::as_str) {
                Some("text_delta") => {
                    let t = d.get("text").and_then(Value::as_str).unwrap_or("");
                    out.push_str(&sse(&translate::chunk(
                        &st.id, st.created, &st.model, json!({ "content": t }), None, None,
                    )));
                }
                Some("input_json_delta") => {
                    let Some(tool) = st.tool_by_block.get(&idx).copied() else { return };
                    let frag = d.get("partial_json").and_then(Value::as_str).unwrap_or("");
                    out.push_str(&sse(&translate::chunk(
                        &st.id, st.created, &st.model,
                        json!({ "tool_calls": [{ "index": tool, "function": { "arguments": frag } }]}),
                        None, None,
                    )));
                }
                _ => {}
            }
        }
        Some("message_delta") => {
            if let Some(u) = ev.get("usage") {
                st.usage = Some(merge_usage(st.usage.take(), u));
            }
            st.stop = ev
                .pointer("/delta/stop_reason")
                .and_then(Value::as_str)
                .map(str::to_string);
            out.push_str(&sse(&translate::chunk(
                &st.id,
                st.created,
                &st.model,
                json!({}),
                Some(translate::finish_reason(st.stop.as_deref())),
                Some(translate::map_usage(st.usage.as_ref())),
            )));
            st.finished = true;
        }
        _ => {}
    }
}

fn stream_response(upstream: wreq::Response, id: String, created: i64, model: String) -> Response {
    let stream = async_stream::stream! {
        let mut st = SseState {
            id: id.clone(),
            created,
            model: model.clone(),
            buf: String::new(),
            usage: None,
            tool_count: 0,
            tool_by_block: HashMap::new(),
            stop: None,
            done: false,
            finished: false,
        };

        let mut upstream = upstream.bytes_stream();
        let mut read_error = None;
        while let Some(item) = upstream.next().await {
            let bytes = match item {
                Ok(b) => b,
                Err(e) => {
                    read_error = Some(e.to_string());
                    break;
                }
            };
            st.buf.push_str(&String::from_utf8_lossy(&bytes));
            let mut out = String::new();
            while let Some(pos) = st.buf.find('\n') {
                let line: String = st.buf.drain(..=pos).collect();
                let Some(raw) = line.strip_prefix("data:") else { continue };
                let raw = raw.trim();
                if raw.is_empty() {
                    continue;
                }
                if raw == "[DONE]" {
                    st.done = true;
                    break;
                }
                handle_event(&mut st, raw, &mut out);
            }
            if !out.is_empty() {
                yield Ok::<_, std::io::Error>(axum::body::Bytes::from(out));
            }
            if st.done {
                break;
            }
        }

        if !st.finished {
            let message = match read_error {
                Some(e) => format!("upstream stream failed before completion: {}", e),
                None => "upstream stream ended before completion".to_string(),
            };
            eprintln!("{}", message);
            yield Ok::<_, std::io::Error>(axum::body::Bytes::from(sse(&json!({
                "error": { "message": message, "type": "upstream_error" }
            }))));
            return;
        }
        yield Ok::<_, std::io::Error>(axum::body::Bytes::from("data: [DONE]\n\n"));
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
        .header(axum::http::header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn merge_usage(a: Option<Value>, b: &Value) -> Value {
    let mut m = a.and_then(|v| v.as_object().cloned()).unwrap_or_default();
    if let Some(ob) = b.as_object() {
        for (k, v) in ob {
            m.insert(k.clone(), v.clone());
        }
    }
    Value::Object(m)
}

fn sse(v: &Value) -> String {
    format!("data: {}\n\n", v)
}

fn json_ok(v: Value) -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&v).unwrap_or_default(),
    )
        .into_response()
}

fn err(code: u16, body: Value) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&body).unwrap_or_default(),
    )
        .into_response()
}

pub fn router(ctx: Ctx) -> axum::Router {
    axum::Router::new()
        .route("/v1/chat/completions", axum::routing::post(chat_completions))
        .with_state(ctx)
}

pub fn init(ctx: Ctx) {
    let app = router(ctx);
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(crate::ADDR).await.unwrap();
        eprintln!("listening on {}", crate::ADDR);
        let _ = axum::serve(listener, app).await;
    });
}