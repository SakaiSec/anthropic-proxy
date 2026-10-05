use anthropic_proxy::Ctx;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

struct Captured {
    auth: String,
    beta: String,
    api_key: Option<String>,
    accept: String,
    body: Value,
    headers: HashMap<String, String>,
}

fn upstream_sse() -> String {
    let evs = [
        json!({"type":"message_start","message":{"usage":{"input_tokens":9}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
        json!({"type":"message_stop"}),
    ];
    evs.iter()
        .map(|e| format!("event: {}\ndata: {}\n\n", e["type"], e))
        .collect::<Vec<_>>()
        .join("")
}

fn upstream_json() -> Value {
    json!({
        "model": "m",
        "content": [{"type":"text","text":"pong"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens":5,"output_tokens":2}
    })
}

fn respond(sock: &mut std::net::TcpStream, status: &str, content_type: &str, payload: &str) {
    sock.write_all(
        format!(
            "HTTP/1.1 {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            status,
            content_type,
            payload.len(),
            payload
        )
        .as_bytes(),
    )
    .ok();
    sock.flush().ok();
}

fn header(lines: &[String], name: &str) -> Option<String> {
    lines
        .iter()
        .find(|l| l.to_ascii_lowercase().starts_with(&format!("{}:", name)))
        .map(|l| l.split(':').nth(1).unwrap().trim().to_string())
}

fn read_request(sock: &mut std::net::TcpStream) -> (String, Vec<String>, Value) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    while let Ok(n) = sock.read(&mut tmp) {
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        let s = String::from_utf8_lossy(&buf).to_string();
        let Some(i) = s.find("\r\n\r\n") else { continue };
        let head = &s[..i];
        let content_length = header(
            &head.lines().map(str::to_string).collect::<Vec<_>>(),
            "content-length",
        )
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
        if buf.len() >= i + 4 + content_length {
            let lines: Vec<String> = head.lines().map(str::to_string).collect();
            let body: Value = serde_json::from_slice(&buf[i + 4..i + 4 + content_length]).unwrap();
            return (lines[0].clone(), lines, body);
        }
    }
    panic!("incomplete request");
}

fn capture(lines: &[String], body: Value) -> Captured {
    Captured {
        auth: header(lines, "authorization").unwrap_or_default(),
        beta: header(lines, "anthropic-beta").unwrap_or_default(),
        api_key: header(lines, "x-api-key"),
        accept: header(lines, "accept").unwrap_or_default(),
        headers: lines
            .iter()
            .skip(1)
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect(),
        body,
    }
}

fn start_upstream(kind: &'static str, expire_first: bool) -> (String, Receiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut seen = 0;
        let mut spent_refresh_tokens = std::collections::HashSet::new();
        for conn in listener.incoming() {
            let Ok(mut sock) = conn else { continue };
            let (request_line, lines, body) = read_request(&mut sock);

            if request_line.contains("/v1/oauth/token") {
                let presented = body["refresh_token"].as_str().unwrap_or_default().to_string();
                if !spent_refresh_tokens.insert(presented) {
                    respond(
                        &mut sock,
                        "400 Bad Request",
                        "application/json",
                        r#"{"error":"invalid_grant"}"#,
                    );
                    continue;
                }
                let payload = json!({
                    "access_token": "fresh-access",
                    "refresh_token": "fresh-refresh",
                    "expires_in": 3600,
                })
                .to_string();
                respond(&mut sock, "200 OK", "application/json", &payload);
                continue;
            }

            assert!(request_line.starts_with("POST /v1/messages"), "{}", request_line);
            seen += 1;
            tx.send(capture(&lines, body)).ok();

            let auth = header(&lines, "authorization").unwrap_or_default();
            if (expire_first && seen == 1) || (kind == "rotate" && auth == "Bearer fake-access") {
                respond(
                    &mut sock,
                    "401 Unauthorized",
                    "application/json",
                    r#"{"type":"error","error":{"type":"authentication_error","message":"expired"}}"#,
                );
            } else if kind == "sse" {
                respond(&mut sock, "200 OK", "text/event-stream", &upstream_sse());
            } else if kind == "truncated_json" {
                respond(&mut sock, "200 OK", "application/json", r#"{"model":"m","content":[{"type":"te"#);
            } else if kind == "truncated_sse" {
                let full = upstream_sse();
                let cut = full.find("event: \"message_delta\"").unwrap();
                respond(&mut sock, "200 OK", "text/event-stream", &full[..cut]);
            } else if kind == "error" {
                respond(
                    &mut sock,
                    "529 Overloaded",
                    "application/json",
                    r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
                );
            } else {
                respond(&mut sock, "200 OK", "application/json", &upstream_json().to_string());
            }
        }
    });
    (format!("http://{}", addr), rx)
}

fn start_proxy(ctx: Ctx) -> String {
    let (addr_tx, addr_rx) = mpsc::channel();
    let app = anthropic_proxy::server::router(ctx);
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                addr_tx.send(listener.local_addr().unwrap()).unwrap();
                axum::serve(listener, app).await.ok();
            });
    });
    let addr = addr_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    format!("http://{}", addr)
}

struct Harness {
    proxy: String,
    creds: std::path::PathBuf,
    seen: Receiver<Captured>,
}

fn harness(tag: &str, kind: &'static str, expire_first: bool) -> Harness {
    let (upstream, seen) = start_upstream(kind, expire_first);
    let dir = std::env::temp_dir().join(format!("apx-{}-{}", tag, std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let creds = dir.join("credentials.json");
    std::fs::write(
        &creds,
        json!({
            "access_token": "fake-access",
            "refresh_token": "fake-refresh",
            "expires_at": 9999999999i64,
            "device_id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        })
        .to_string(),
    )
    .unwrap();
    let proxy = start_proxy(Ctx {
        creds: creds.clone(),
        upstream: upstream.clone(),
        token_url: format!("{}/v1/oauth/token", upstream),
    });
    Harness { proxy, creds, seen }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(dir) = self.creds.parent() {
            std::fs::remove_dir_all(dir).ok();
        }
    }
}

fn wait(h: &Harness) -> Captured {
    h.seen.recv_timeout(std::time::Duration::from_secs(15)).expect("upstream saw no request")
}

fn dechunk(body: &str) -> String {
    if !body.starts_with(|c: char| c.is_ascii_hexdigit()) {
        return body.to_string();
    }
    let mut out = String::new();
    let mut rest = body;
    while let Some((size_line, tail)) = rest.split_once("\r\n") {
        let Some(hex) = size_line.split(';').next() else { break };
        let Ok(size) = usize::from_str_radix(hex.trim(), 16) else { break };
        if size == 0 || tail.len() < size {
            out.push_str(&tail[..size.min(tail.len())]);
            break;
        }
        out.push_str(&tail[..size]);
        rest = tail[size..].trim_start_matches("\r\n");
    }
    out
}

fn post_with(url: &str, body: &Value, extra: &str) -> (u16, String) {
    let addr = url.trim_start_matches("http://").split('/').next().unwrap().to_string();
    let mut sock = std::net::TcpStream::connect(&addr).unwrap();
    let payload = body.to_string();
    sock.write_all(
        format!(
            "POST /v1/chat/completions HTTP/1.1\r\nhost: {}\r\ncontent-type: application/json\r\n{}content-length: {}\r\nconnection: close\r\n\r\n{}",
            addr,
            extra,
            payload.len(),
            payload
        )
        .as_bytes(),
    )
    .unwrap();
    let mut raw = String::new();
    sock.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let status: u16 = head.lines().next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, dechunk(body))
}

fn post(url: &str, body: &Value) -> (u16, String) {
    post_with(url, body, "")
}

#[test]
fn non_streaming_round_trip() {
    let h = harness("nonstream", "json", false);
    let (status, body) = post_with(
        &h.proxy,
        &json!({"model":"m","messages":[
            {"role":"system","content":"sys"},
            {"role":"user","content":"ping"}
        ]}),
        "x-claude-code-session-id: sess-1\r\n",
    );
    let cap = wait(&h);
    assert_eq!(status, 200, "body={}", body);

    let out: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(out["object"], json!("chat.completion"));
    assert_eq!(out["choices"][0]["message"]["role"], json!("assistant"));
    assert_eq!(out["choices"][0]["message"]["content"], json!("pong"));
    assert_eq!(out["choices"][0]["finish_reason"], json!("stop"));
    assert_eq!(out["choices"][0]["index"], json!(0));
    assert_eq!(out["usage"]["prompt_tokens"], json!(5));
    assert_eq!(out["usage"]["completion_tokens"], json!(2));
    assert_eq!(out["usage"]["total_tokens"], json!(7));

    assert_eq!(cap.auth, "Bearer fake-access");
    assert!(cap.beta.contains("oauth-2025-04-20"), "beta={}", cap.beta);
    assert!(cap.api_key.is_none(), "x-api-key must not be sent for OAuth");
    assert_eq!(cap.accept, "application/json");
    assert!(
        cap.body["system"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("x-anthropic-billing-header: cc_version="),
        "system[0] is the billing block, got {:?}",
        cap.body["system"][0]
    );
    assert_eq!(cap.body["system"][0].get("cache_control"), None::<&Value>);
    assert_eq!(
        cap.body["system"][1],
        json!({
            "type": "text",
            "text": "You are Claude Code, Anthropic's official CLI for Claude.",
            "cache_control": anthropic_proxy::cache_breakpoint(),
        })
    );
    assert_eq!(cap.body["system"].as_array().unwrap().len(), 2, "caller system leaves the system array");
    assert_eq!(cap.body["max_tokens"], json!(4096));

    let msgs = cap.body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "caller system rides in messages: {:?}", msgs);
    assert_eq!(msgs[0]["role"], json!("user"));
    assert_eq!(msgs[1]["role"], json!("system"));
    assert_eq!(msgs[1]["content"][0]["text"], json!("sys"));
    assert_eq!(msgs[1]["content"][0]["cache_control"], anthropic_proxy::cache_breakpoint());
    let first_user = msgs[0]["content"].as_array().unwrap();
    assert!(
        first_user[0]["text"].as_str().unwrap().contains("# currentDate"),
        "currentDate reminder leads the first user turn: {:?}",
        first_user
    );
    assert_eq!(first_user[1]["text"], json!("ping"));
    let uid: Value = serde_json::from_str(cap.body["metadata"]["user_id"].as_str().unwrap()).unwrap();
    assert_eq!(uid["session_id"], json!("sess-1"));
    assert_eq!(uid["device_id"], json!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"));
}

#[test]
fn streaming_emits_openai_chunks() {
    let h = harness("stream", "sse", false);
    let (status, body) = post(
        &h.proxy,
        &json!({"model":"m","stream":true,"messages":[{"role":"user","content":"ping"}]}),
    );
    let cap = wait(&h);
    assert_eq!(status, 200);

    let chunks: Vec<Value> = body
        .lines()
        .filter_map(|l| l.strip_prefix("data:").map(str::trim))
        .filter(|d| *d != "[DONE]")
        .map(|d| serde_json::from_str(d).unwrap())
        .collect();

    assert!(chunks.len() >= 4, "chunks={}", body);
    assert_eq!(chunks[0]["object"], json!("chat.completion.chunk"));
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], json!("assistant"));
    let ids: Vec<_> = chunks.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(ids[0], ids[ids.len() - 1]);

    let text: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "Hello world");

    let last = chunks.last().unwrap();
    assert_eq!(last["choices"][0]["finish_reason"], json!("stop"));
    assert_eq!(last["usage"]["prompt_tokens"], json!(9));
    assert_eq!(last["usage"]["completion_tokens"], json!(4));
    assert_eq!(last["usage"]["total_tokens"], json!(13));

    assert!(body.trim_end().ends_with("data: [DONE]"), "body={}", body);

    assert_eq!(cap.body["stream"], json!(true));
    assert_eq!(cap.accept, "text/event-stream");
}

#[test]
fn upstream_sees_the_claude_code_fingerprint() {
    let h = harness("fp", "json", false);
    let (status, _) = post_with(
        &h.proxy,
        &json!({"model":"m","messages":[{"role":"user","content":"ping"}]}),
        "x-claude-code-session-id: sess-fp\r\n",
    );
    let cap = wait(&h);
    assert_eq!(status, 200);
    for name in [
        "x-stainless-lang",
        "x-stainless-package-version",
        "x-stainless-runtime",
        "x-stainless-runtime-version",
        "x-stainless-os",
        "x-stainless-arch",
        "x-stainless-retry-count",
        "x-stainless-timeout",
        "x-app",
        "x-claude-code-session-id",
    ] {
        assert!(
            cap.headers.contains_key(name),
            "missing {}; upstream saw {:?}",
            name,
            cap.headers.keys().collect::<Vec<_>>()
        );
    }
    assert_eq!(cap.headers["x-stainless-lang"], "js");
    assert_eq!(cap.headers["x-stainless-runtime"], "node");
    assert_eq!(cap.headers["x-stainless-retry-count"], "0");
    assert_eq!(cap.headers["x-claude-code-session-id"], "sess-fp");
    assert_eq!(cap.headers["user-agent"], anthropic_proxy::CC_USER_AGENT);
    assert_eq!(cap.headers["anthropic-version"], "2023-06-01");
    assert_eq!(cap.headers["anthropic-dangerous-direct-browser-access"], "true");

    let billing = cap.body["system"][0]["text"].as_str().unwrap().to_string();
    assert!(billing.starts_with("x-anthropic-billing-header: "), "{}", billing);
    assert!(billing.contains("cc_version="), "{}", billing);
    assert!(billing.contains("cc_entrypoint=cli;"), "{}", billing);
}

#[test]
fn session_header_is_generated_when_the_client_omits_one() {
    let h = harness("nosess", "json", false);
    post(&h.proxy, &json!({"model":"m","messages":[{"role":"user","content":"x"}]}));
    let cap = wait(&h);
    assert!(
        cap.headers["x-claude-code-session-id"].len() >= 24,
        "expected a generated session id, got {:?}",
        cap.headers["x-claude-code-session-id"]
    );
    let uid: Value = serde_json::from_str(cap.body["metadata"]["user_id"].as_str().unwrap()).unwrap();
    assert_eq!(uid["session_id"], json!(cap.headers["x-claude-code-session-id"]));
}

#[test]
fn request_without_a_model_is_rejected_before_upstream() {
    let h = harness("nomodel", "json", false);
    let (status, body) = post(
        &h.proxy,
        &json!({"messages":[{"role":"user","content":"x"}]}),
    );
    assert_eq!(status, 400, "body={}", body);
    assert!(body.contains("missing model"), "body={}", body);
    assert!(
        h.seen.try_recv().is_err(),
        "a request with no model must never reach upstream"
    );
}

#[test]
fn upstream_error_is_relayed_verbatim() {
    let h = harness("error", "error", false);
    let (status, body) = post(
        &h.proxy,
        &json!({"model":"m","messages":[{"role":"user","content":"x"}]}),
    );
    wait(&h);
    assert_eq!(status, 529);
    assert!(body.contains("overloaded_error"), "body={}", body);
}

#[test]
fn streaming_error_is_relayed_as_json_not_sse() {
    let h = harness("streamerror", "error", false);
    let (status, body) = post(
        &h.proxy,
        &json!({"model":"m","stream":true,"messages":[{"role":"user","content":"x"}]}),
    );
    wait(&h);
    assert_eq!(status, 529);
    assert!(!body.contains("data:"), "must not be SSE: {}", body);
    assert!(body.contains("overloaded_error"), "body={}", body);
}

#[test]
fn stale_access_token_triggers_refresh_and_retry() {
    let h = harness("refresh", "json", true);
    let (status, body) = post(
        &h.proxy,
        &json!({"model":"m","messages":[{"role":"user","content":"x"}]}),
    );
    let first = wait(&h);
    assert_eq!(first.auth, "Bearer fake-access", "first attempt uses the stale token");
    let retried = wait(&h);
    assert_eq!(status, 200, "body={}", body);
    let out: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(out["choices"][0]["message"]["content"], json!("pong"));
    assert_eq!(retried.auth, "Bearer fresh-access", "retry must use the refreshed token");

    let creds: Value =
        serde_json::from_str(&std::fs::read_to_string(&h.creds).unwrap()).unwrap();
    assert_eq!(creds["access_token"], json!("fresh-access"));
    assert_eq!(creds["refresh_token"], json!("fresh-refresh"));
    assert_eq!(
        creds["device_id"],
        json!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
        "device_id must survive the refresh"
    );
}

#[test]
fn missing_credentials_return_an_error() {
    let (upstream, _seen) = start_upstream("json", false);
    let proxy = start_proxy(Ctx {
        creds: std::env::temp_dir().join("apx-absent/credentials.json"),
        upstream: upstream.clone(),
        token_url: format!("{}/v1/oauth/token", upstream),
    });
    std::fs::remove_file(std::env::temp_dir().join("apx-absent/credentials.json")).ok();
    let (status, body) = post(
        &proxy,
        &json!({"model":"m","messages":[{"role":"user","content":"x"}]}),
    );
    assert_eq!(status, 502);
    assert!(body.contains("no credentials"), "body={}", body);
}

#[test]
fn concurrent_requests_get_distinct_ids() {
    let h = harness("concurrent", "json", false);
    let proxy = Arc::new(h.proxy.clone());
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let p = proxy.clone();
            std::thread::spawn(move || {
                post(&p, &json!({"model":"m","messages":[{"role":"user","content":"x"}]}))
            })
        })
        .collect();
    let mut ids = Vec::new();
    for handle in handles {
        let (status, body) = handle.join().unwrap();
        assert_eq!(status, 200, "body={}", body);
        ids.push(serde_json::from_str::<Value>(&body).unwrap()["id"].as_str().unwrap().to_string());
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 4, "ids should be unique per request: {:?}", ids);
    for _ in 0..4 {
        wait(&h);
    }
}

#[test]
fn concurrent_401s_refresh_once() {
    let h = harness("rotate", "rotate", false);
    let proxy = Arc::new(h.proxy.clone());
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let p = proxy.clone();
            std::thread::spawn(move || {
                post(&p, &json!({"model":"m","messages":[{"role":"user","content":"x"}]}))
            })
        })
        .collect();
    for handle in handles {
        let (status, body) = handle.join().unwrap();
        assert_eq!(status, 200, "losers of the refresh race must reuse the winner's token: {}", body);
    }
    let creds: Value =
        serde_json::from_str(&std::fs::read_to_string(&h.creds).unwrap()).unwrap();
    assert_eq!(creds["access_token"], json!("fresh-access"));
}

#[test]
fn expired_token_is_refreshed_before_sending() {
    let h = harness("proactive", "json", false);
    let mut creds: Value =
        serde_json::from_str(&std::fs::read_to_string(&h.creds).unwrap()).unwrap();
    creds["expires_at"] = json!(0);
    std::fs::write(&h.creds, creds.to_string()).unwrap();
    let (status, body) = post(
        &h.proxy,
        &json!({"model":"m","messages":[{"role":"user","content":"x"}]}),
    );
    assert_eq!(status, 200, "body={}", body);
    assert_eq!(wait(&h).auth, "Bearer fresh-access", "no request should go out on the expired token");
}

#[test]
fn truncated_upstream_json_is_a_502_not_an_empty_completion() {
    let h = harness("truncjson", "truncated_json", false);
    let (status, body) = post(
        &h.proxy,
        &json!({"model":"m","messages":[{"role":"user","content":"x"}]}),
    );
    wait(&h);
    assert_eq!(status, 502, "body={}", body);
    assert!(body.contains("upstream_error"), "body={}", body);
}

#[test]
fn truncated_upstream_stream_reports_an_error() {
    let h = harness("truncsse", "truncated_sse", false);
    let (status, body) = post(
        &h.proxy,
        &json!({"model":"m","stream":true,"messages":[{"role":"user","content":"x"}]}),
    );
    wait(&h);
    assert_eq!(status, 200);
    assert!(body.contains("Hel"), "partial content is still relayed: {}", body);
    assert!(body.contains("upstream_error"), "body={}", body);
    assert!(!body.contains("\"finish_reason\":\"stop\""), "must not fake a clean stop: {}", body);
    assert!(!body.contains("[DONE]"), "body={}", body);
}

#[test]
fn malformed_tool_is_rejected() {
    let h = harness("badtool", "json", false);
    let (status, body) = post(
        &h.proxy,
        &json!({"model":"m","tools":[{"type":"function"}],"messages":[{"role":"user","content":"x"}]}),
    );
    assert_eq!(status, 400, "body={}", body);
    assert!(body.contains("tools[0]"), "body={}", body);
}
