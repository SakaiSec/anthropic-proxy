pub mod oauth;
pub mod server;
pub mod translate;

use rand::RngExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const CRED_FILE: &str = "credentials.json";
const CC_SYSTEM: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

pub fn cache_breakpoint() -> Value {
    json!({ "type": "ephemeral", "ttl": "1h" })
}

pub const CC_USER_AGENT: &str = "claude-cli/2.1.289 (external, cli)";
pub const CC_VERSION: &str = "2.1.289";
pub const SDK_VERSION: &str = "0.112.1";
pub const NODE_VERSION: &str = "v26.3.0";

fn build_fingerprint(message: &str, version: &str) -> String {
    let units: Vec<u16> = message.encode_utf16().collect();
    let sampled: Vec<u16> = [4, 7, 20]
        .iter()
        .map(|i| units.get(*i).copied().unwrap_or(u16::from(b'0')))
        .collect();
    let input = format!("59cf53e54c78{}{}", String::from_utf16_lossy(&sampled), version);
    let digest = Sha256::digest(input.as_bytes());
    digest.iter().map(|b| format!("{:02x}", b)).collect::<String>()[..3].to_string()
}

fn first_user_text(claude: &Value) -> String {
    let Some(messages) = claude.get("messages").and_then(Value::as_array) else {
        return String::new();
    };
    for m in messages.iter().filter(|m| m.get("role").and_then(Value::as_str) == Some("user")) {
        match m.get("content") {
            Some(Value::String(s)) if !s.is_empty() => return s.clone(),
            Some(Value::Array(blocks)) => {
                if let Some(t) = blocks.iter().find_map(|b| {
                    (b.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| b.get("text").and_then(Value::as_str))
                        .flatten()
                        .filter(|t| !t.is_empty())
                }) {
                    return t.to_string();
                }
            }
            _ => {}
        }
    }
    String::new()
}

pub fn billing_header(claude: &Value) -> String {
    format!(
        "x-anthropic-billing-header: cc_version={}.{}; cc_entrypoint=cli;",
        CC_VERSION,
        build_fingerprint(&first_user_text(claude), CC_VERSION)
    )
}

pub const ADDR: &str = "127.0.0.1:31701";
pub const UPSTREAM: &str = "https://api.anthropic.com";

#[derive(Clone)]
pub struct Ctx {
    pub creds: PathBuf,
    pub upstream: String,
    pub token_url: String,
}

impl Ctx {
    pub fn production() -> Self {
        Self {
            creds: config_dir().join(CRED_FILE),
            upstream: UPSTREAM.into(),
            token_url: oauth::TOKEN_URL.into(),
        }
    }
}

pub fn config_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "anthropic-proxy")
        .map(|p| p.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn client() -> &'static wreq::Client {
    static C: OnceLock<wreq::Client> = OnceLock::new();
    C.get_or_init(|| {
        wreq::Client::builder()
            .emulation(wreq_util::Emulation::Chrome136)
            .build()
            .unwrap()
    })
}

pub fn http_client() -> &'static wreq::Client {
    client()
}

pub fn rand_id() -> String {
    let mut rng = rand::rng();
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    (0..24)
        .map(|_| char::from(ALPHABET[rng.random_range(0..ALPHABET.len())]))
        .collect()
}

pub fn device_id() -> String {
    let mut rng = rand::rng();
    let h: String = (0..16)
        .map(|_| format!("{:02x}", rng.random::<u8>()))
        .collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn read_store(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or(Value::Null)
}

fn read_store_checked(path: &Path) -> Result<Value, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("no credentials at {}", path.display()))
        }
        Err(e) => return Err(format!("{}: {}", path.display(), e)),
    };
    serde_json::from_str(&raw)
        .map_err(|e| format!("unreadable credentials at {}: {}", path.display(), e))
}

pub fn stored_device_id(path: &Path) -> String {
    read_store(path)
        .get("device_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(device_id)
}

pub fn save_token(path: &Path, t: &Value) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
    }
    let out = json!({
        "access_token": t.get("access_token").and_then(Value::as_str).unwrap_or_default(),
        "refresh_token": t.get("refresh_token").and_then(Value::as_str).unwrap_or_default(),
        "expires_at": now() + t.get("expires_in").and_then(Value::as_i64).unwrap_or(3600),
        "device_id": stored_device_id(path),
    });
    write_private(path, &out)
}

fn write_private(path: &Path, out: &Value) -> Result<(), String> {
    use std::io::Write;
    let body = serde_json::to_string_pretty(out).map_err(|e| e.to_string())?;
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".{}.{}.tmp", std::process::id(), rand_id()));
    let tmp = path.with_file_name(tmp_name);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = opts
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(body.as_bytes())?;
            f.sync_all()
        })
        .and_then(|_| std::fs::rename(&tmp, path));
    if result.is_err() {
        std::fs::remove_file(&tmp).ok();
    }
    result.map_err(|e| e.to_string())
}

pub fn load_token(path: &Path) -> Result<Token, String> {
    let v = read_store_checked(path)?;
    Ok(Token {
        access_token: v.get("access_token").and_then(Value::as_str).unwrap_or_default().into(),
        refresh_token: v.get("refresh_token").and_then(Value::as_str).unwrap_or_default().into(),
        expires_at: v.get("expires_at").and_then(Value::as_i64),
    })
}

#[derive(Debug)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: Option<i64>,
}

const EXPIRY_SKEW: i64 = 60;

impl Token {
    pub fn is_expiring(&self) -> bool {
        self.expires_at.is_some_and(|at| at <= now() + EXPIRY_SKEW)
    }
}

pub async fn refresh_and_save(ctx: &Ctx, refresh_token: &str) -> Result<(), String> {
    let t = oauth::refresh(&ctx.token_url, refresh_token).await?;
    save_token(&ctx.creds, &t)
}

pub async fn refresh_token(ctx: &Ctx, rejected: Option<&str>) -> Result<Token, String> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = LOCK.lock().await;
    let current = load_token(&ctx.creds)?;
    let stale = match rejected {
        Some(old) => current.access_token == old,
        None => current.is_expiring(),
    };
    if !stale {
        return Ok(current);
    }
    refresh_and_save(ctx, &current.refresh_token).await?;
    load_token(&ctx.creds)
}

pub fn cloaked_system(mut claude: Value, session_id: &str, creds: &Path) -> Value {
    let caller_system = claude
        .get("system")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let blocks = vec![
        json!({"type": "text", "text": billing_header(&claude)}),
        json!({"type": "text", "text": CC_SYSTEM, "cache_control": cache_breakpoint()}),
    ];
    claude
        .as_object_mut()
        .unwrap()
        .insert("system".into(), Value::Array(blocks));

    if let Some(text) = caller_system {
        relocate_system_turn(&mut claude, &text);
    }
    stamp_current_date(&mut claude);
    claude.as_object_mut().unwrap().insert(
        "metadata".into(),
        json!({
            "user_id": json!({
                "device_id": stored_device_id(creds),
                "account_uuid": "00000000-0000-0000-0000-000000000000",
                "session_id": session_id,
            })
            .to_string()
        }),
    );
    claude
}

fn relocate_system_turn(claude: &mut Value, text: &str) {
    let Some(messages) = claude
        .get_mut("messages")
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    let Some(first_user) = messages
        .iter()
        .position(|m| m.get("role").and_then(Value::as_str) == Some("user"))
    else {
        return;
    };
    let mut insert_at = first_user + 1;
    while insert_at < messages.len()
        && messages[insert_at].get("role").and_then(Value::as_str) == Some("user")
    {
        insert_at += 1;
    }
    let already_present = messages
        .get(insert_at..insert_at.saturating_add(1))
        .is_some_and(|m| {
            m.len() == 1
                && m[0].get("role").and_then(Value::as_str) == Some("system")
                && m[0].get("content").and_then(Value::as_str) == Some(text)
        });
    if !already_present {
        messages.insert(
            insert_at,
            json!({
                "role": "system",
                "content": [{
                    "type": "text",
                    "text": text,
                    "cache_control": cache_breakpoint(),
                }],
            }),
        );
    }
}

fn stamp_current_date(claude: &mut Value) {
    const REMINDER: &str = "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# currentDate\nToday's date is %s.\n\n      IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.\n</system-reminder>\n\n";
    let Some(messages) = claude.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let Some(idx) = messages
        .iter()
        .position(|m| m.get("role").and_then(Value::as_str) == Some("user"))
    else {
        return;
    };
    let reminder = json!({"type": "text", "text": REMINDER.replace("%s", &local_date())});
    let content = messages[idx].get_mut("content").unwrap();
    match content {
        Value::String(s) => {
            let user = json!({"type": "text", "text": s.clone(), "cache_control": cache_breakpoint()});
            *content = Value::Array(vec![reminder, user]);
        }
        Value::Array(blocks) => {
            blocks.insert(0, reminder);
            if let Some(last) = blocks.last_mut().and_then(|b| b.as_object_mut()) {
                last.entry("cache_control").or_insert_with(cache_breakpoint);
            }
        }
        _ => {}
    }
}

fn local_date() -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let secs = crate::now();
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let weekday = (days + 4).rem_euclid(7) as usize;
    format!(
        "{} {}, {:02} {} at {:02}:{:02} UTC",
        DAYS[weekday],
        MONTHS[(m - 1) as usize],
        d,
        y,
        tod / 3600,
        (tod % 3600) / 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub async fn login(ctx: &Ctx) {
    let url = oauth::authorize_url();
    let state = oauth::PENDING
        .with(|p| p.borrow().as_ref().map(|(_, s)| s.clone()))
        .unwrap_or_default();
    let callback = match oauth::callback_server(state).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{}", e);
            return;
        }
    };
    eprintln!("open:\n{}", url);
    let code = match callback.code.await {
        Ok(c) => c,
        Err(_) => {
            eprintln!("no callback received");
            return;
        }
    };
    callback.server.abort();
    match oauth::exchange_code(ctx, &code).await {
        Ok(t) => {
            if let Err(e) = save_token(&ctx.creds, &t) {
                eprintln!("save failed: {}", e);
            }
        }
        Err(e) => eprintln!("exchange failed: {}", e),
    }
}

const USAGE: &str = "usage: anthropic-proxy [serve | login | status]";

pub fn run(args: &[String]) {
    let ctx = Ctx::production();
    match args.get(1).map(String::as_str).unwrap_or("serve") {
        "serve" => server::init(ctx),
        "login" => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(login(&ctx)),
        "status" => match load_token(&ctx.creds) {
            Ok(t) => eprintln!("{}", redact(&t.access_token)),
            Err(e) => eprintln!("{}", e),
        },
        "help" | "-h" | "--help" => println!("{}", USAGE),
        other => {
            eprintln!("unknown command: {}\n{}", other, USAGE);
            std::process::exit(2);
        }
    }
}

fn redact(token: &str) -> String {
    const HEAD: usize = 6;
    const TAIL: usize = 4;
    let n = token.chars().count();
    if n == 0 {
        return "not logged in".to_string();
    }
    if n < 2 * (HEAD + TAIL) {
        return "logged in (token redacted)".to_string();
    }
    let head: String = token.chars().take(HEAD).collect();
    let tail: String = token.chars().skip(n - TAIL).collect();
    format!("logged in as {}…{}", head, tail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    #[test]
    fn system_role_is_hoisted() {
        let req = json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "user", "content": "hi"}
            ]
        });
        let out = translate::openai_to_claude(&req).unwrap();
        assert_eq!(out["system"], json!("be terse"));
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
        assert_eq!(out["messages"][0]["role"], json!("user"));
        assert_eq!(out["messages"][0]["content"][0]["text"], json!("hi"));
        assert_eq!(out["max_tokens"], json!(4096));
    }

    #[test]
    fn cache_breakpoint_lands_on_the_stable_prefix() {
        let req = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "one"},
                {"role": "assistant", "content": "two"},
                {"role": "user", "content": "three"}
            ]
        });
        let out = translate::openai_to_claude(&req).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert!(msgs[0]["content"][0].get("cache_control").is_none());
        assert_eq!(msgs[1]["content"][0]["cache_control"], crate::cache_breakpoint());
        assert!(msgs[2]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn a_single_turn_gets_no_history_breakpoint() {
        let req = json!({"model": "m", "messages": [{"role": "user", "content": "only"}]});
        let out = translate::openai_to_claude(&req).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert!(msgs[0]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn model_is_passed_through_verbatim() {
        for name in ["claude-sonnet-4-5-20250929", "claude-opus-4-1", "some-alias"] {
            let req = json!({
                "model": name,
                "messages": [{"role": "user", "content": "x"}]
            });
            assert_eq!(translate::openai_to_claude(&req).unwrap()["model"], json!(name));
        }
    }

    #[test]
    fn missing_or_blank_model_is_rejected() {
        let msgs = json!([{"role": "user", "content": "x"}]);
        assert!(translate::openai_to_claude(&json!({"messages": msgs})).is_err());
        assert!(translate::openai_to_claude(&json!({"model": "", "messages": msgs})).is_err());
        assert!(translate::openai_to_claude(&json!({"model": "   ", "messages": msgs})).is_err());
        assert!(translate::openai_to_claude(&json!({"model": 7, "messages": msgs})).is_err());
    }

    #[test]
    fn max_tokens_passes_through() {
        let req = json!({
            "model": "m",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "x"}]
        });
        assert_eq!(translate::openai_to_claude(&req).unwrap()["max_tokens"], json!(100));
    }

    #[test]
    fn consecutive_same_role_messages_merge() {
        let req = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "a"},
                {"role": "user", "content": "b"}
            ]
        });
        let out = translate::openai_to_claude(&req).unwrap();
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
        assert_eq!(out["messages"][0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn tool_result_follows_tool_use() {
        let req = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "get", "arguments": "{\"q\":1}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "42"}
            ]
        });
        let out = translate::openai_to_claude(&req).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["content"][0]["type"], json!("tool_use"));
        assert_eq!(msgs[1]["content"][0]["input"], json!({"q": 1}));
        assert_eq!(msgs[2]["role"], json!("user"));
        assert_eq!(msgs[2]["content"][0]["type"], json!("tool_result"));
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], json!("call_1"));
    }

    #[test]
    fn tools_and_tool_choice_convert() {
        let req = json!({
            "model": "m",
            "tool_choice": "required",
            "tools": [{"type": "function", "function": {
                "name": "f", "description": "d",
                "parameters": {"type": "object", "properties": {"a": {"type": "string"}}}
            }}],
            "messages": [{"role": "user", "content": "x"}]
        });
        let out = translate::openai_to_claude(&req).unwrap();
        assert_eq!(out["tools"][0]["name"], json!("f"));
        assert_eq!(out["tools"][0]["input_schema"]["properties"]["a"]["type"], json!("string"));
        assert_eq!(out["tool_choice"], json!({"type": "any"}));
    }

    #[test]
    fn mcp_tool_names_are_rewritten_on_the_way_out() {
        assert_eq!(translate::mcp_name_out("mcp_servers"), "mcp_Servers");
        assert_eq!(translate::mcp_name_out("mcp_notion_search"), "mcp_Notion_search");
        assert_eq!(translate::mcp_name_out("bash"), "bash");
        assert_eq!(translate::mcp_name_out("codegraph_explore"), "codegraph_explore");
        assert_eq!(translate::mcp_name_out("mcp_Bash"), "mcp_Bash", "already PascalCase");
        assert_eq!(translate::mcp_name_out("mcp__servers"), "mcp__servers", "empty segment");
        assert_eq!(translate::mcp_name_out("mcp_"), "mcp_");
    }

    #[test]
    fn mcp_tool_names_are_restored_on_the_way_back() {
        assert_eq!(translate::mcp_name_in("mcp_Servers"), "mcp_servers");
        assert_eq!(translate::mcp_name_in("mcp_Notion_search"), "mcp_notion_search");
        assert_eq!(translate::mcp_name_in("bash"), "bash");
        assert_eq!(translate::mcp_name_in("mcp_Bash"), "mcp_bash", "idempotent round trip");
    }

    #[test]
    fn lowercase_mcp_tools_do_not_reach_upstream() {
        let req = json!({
            "model": "m",
            "tools": [
                {"type": "function", "function": {"name": "mcp_notion_search"}},
                {"type": "function", "function": {"name": "bash"}}
            ],
            "tool_choice": {"type": "function", "function": {"name": "mcp_notion_search"}},
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "c1", "type": "function",
                     "function": {"name": "mcp_notion_search", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": "ok"}
            ]
        });
        let out = translate::openai_to_claude(&req).unwrap();
        assert_eq!(out["tools"][0]["name"], json!("mcp_Notion_search"));
        assert_eq!(out["tools"][1]["name"], json!("bash"));
        assert_eq!(out["tool_choice"]["name"], json!("mcp_Notion_search"));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["content"][0]["name"], json!("mcp_Notion_search"));
    }

    #[test]
    fn streamed_mcp_tool_names_come_back_lowercase() {
        let mut st = state();
        let mut out = String::new();
        server::handle_event(
            &mut st,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu","name":"mcp_Notion_search"}}"#,
            &mut out,
        );
        let chunks = parse(&out);
        assert_eq!(chunks[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"], json!("mcp_notion_search"));
    }

    #[test]
    fn non_streamed_mcp_tool_names_come_back_lowercase() {
        let claude = json!({
            "model": "m",
            "content": [{"type": "tool_use", "id": "tu", "name": "mcp_Notion_search", "input": {}}],
            "stop_reason": "tool_use"
        });
        let out = translate::claude_to_openai(&claude, "id", 0);
        assert_eq!(out["choices"][0]["message"]["tool_calls"][0]["function"]["name"], json!("mcp_notion_search"));
    }

    #[test]
    fn tool_choice_none_maps() {
        let req = json!({
            "model": "m", "tool_choice": "none",
            "messages": [{"role": "user", "content": "x"}]
        });
        assert_eq!(translate::openai_to_claude(&req).unwrap()["tool_choice"], json!({"type": "none"}));
    }

    #[test]
    fn base64_image_converts() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "what"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}}
            ]}]
        });
        let out = translate::openai_to_claude(&req).unwrap();
        let blocks = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[1]["source"]["media_type"], json!("image/png"));
        assert_eq!(blocks[1]["source"]["data"], json!("AAA"));
    }

    #[test]
    fn non_data_url_image_is_rejected() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
            ]}]
        });
        assert!(translate::openai_to_claude(&req).is_err());
    }

    #[test]
    fn claude_response_to_openai_chat() {
        let claude = json!({
            "model": "m",
            "content": [
                {"type": "text", "text": "hello "},
                {"type": "text", "text": "world"},
                {"type": "tool_use", "id": "tu_1", "name": "f", "input": {"a": 1}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 3}
        });
        let out = translate::claude_to_openai(&claude, "chatcmpl-x", 100);
        assert_eq!(out["object"], json!("chat.completion"));
        assert_eq!(out["choices"][0]["message"]["content"], json!("hello world"));
        assert_eq!(out["choices"][0]["finish_reason"], json!("tool_calls"));
        let tc = &out["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tc["index"], json!(0));
        assert_eq!(tc["function"]["name"], json!("f"));
        assert_eq!(tc["function"]["arguments"], json!("{\"a\":1}"));
        assert_eq!(out["usage"]["prompt_tokens"], json!(10));
        assert_eq!(out["usage"]["completion_tokens"], json!(3));
        assert_eq!(out["usage"]["total_tokens"], json!(13));
    }

    #[test]
    fn thinking_blocks_are_dropped() {
        let claude = json!({
            "content": [
                {"type": "thinking", "thinking": "secret"},
                {"type": "text", "text": "answer"}
            ],
            "stop_reason": "end_turn"
        });
        let out = translate::claude_to_openai(&claude, "id", 0);
        assert_eq!(out["choices"][0]["message"]["content"], json!("answer"));
        assert_eq!(out["choices"][0]["finish_reason"], json!("stop"));
    }

    #[test]
    fn finish_reason_mapping() {
        assert_eq!(translate::finish_reason(Some("end_turn")), "stop");
        assert_eq!(translate::finish_reason(Some("max_tokens")), "length");
        assert_eq!(translate::finish_reason(Some("tool_use")), "tool_calls");
        assert_eq!(translate::finish_reason(Some("stop_sequence")), "stop");
        assert_eq!(translate::finish_reason(None), "stop");
    }

    fn state() -> server::SseState {
        server::SseState {
            id: "chatcmpl-x".into(),
            created: 0,
            model: "m".into(),
            buf: String::new(),
            usage: None,
            tool_count: 0,
            tool_by_block: std::collections::HashMap::new(),
            stop: None,
            done: false,
            finished: false,
        }
    }

    fn parse(out: &str) -> Vec<Value> {
        out.lines()
            .filter_map(|l| l.strip_prefix("data:").map(str::trim))
            .filter(|d| *d != "[DONE]")
            .map(|d| serde_json::from_str(d).unwrap())
            .collect()
    }

    #[test]
    fn sse_events_produce_openai_chunks() {
        let mut st = state();
        let events = [
            r#"{"type":"message_start","message":{"usage":{"input_tokens":7}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"he"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"llo"}}"#,
        ];
        let mut out = String::new();
        for e in events {
            server::handle_event(&mut st, e, &mut out);
        }
        let chunks = parse(&out);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], json!("assistant"));
        assert_eq!(chunks[1]["choices"][0]["delta"]["content"], json!("he"));
        assert_eq!(chunks[2]["choices"][0]["delta"]["content"], json!("llo"));
        assert_eq!(st.usage.as_ref().unwrap()["input_tokens"], json!(7));

        let mut out = String::new();
        server::handle_event(
            &mut st,
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":5}}"#,
            &mut out,
        );
        let last = &parse(&out)[0];
        assert!(last["choices"][0]["delta"].as_object().unwrap().is_empty());
        assert_eq!(last["choices"][0]["finish_reason"], json!("length"));
        assert!(st.finished);
        assert_eq!(last["usage"]["prompt_tokens"], json!(7));
        assert_eq!(last["usage"]["completion_tokens"], json!(5));
        assert_eq!(last["usage"]["total_tokens"], json!(12));
    }

    #[test]
    fn streaming_tool_calls_use_block_index_mapping() {
        let mut st = state();
        let mut out = String::new();
        server::handle_event(
            &mut st,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu","name":"f"}}"#,
            &mut out,
        );
        server::handle_event(
            &mut st,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1}"}}"#,
            &mut out,
        );
        let chunks = parse(&out);
        assert_eq!(chunks[0]["choices"][0]["delta"]["tool_calls"][0]["index"], json!(0));
        assert_eq!(chunks[0]["choices"][0]["delta"]["tool_calls"][0]["id"], json!("tu"));
        assert_eq!(chunks[1]["choices"][0]["delta"]["tool_calls"][0]["index"], json!(0));
        assert_eq!(
            chunks[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            json!("{\"a\":1}")
        );
    }

    #[test]
    fn cloaking_injects_system_and_metadata() {
        let claude = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "x"}],
            "system": "caller prompt"
        });
        let creds = std::env::temp_dir().join("apx-cloak-test/credentials.json");
        let out = cloaked_system(claude, "sess-1", &creds);
        let sys = out["system"].as_array().unwrap();
        assert_eq!(sys.len(), 2);
        assert!(
            sys[0]["text"].as_str().unwrap().starts_with("x-anthropic-billing-header:"),
            "system[0] must be the billing block, got {:?}",
            sys[0]
        );
        assert_eq!(sys[1], json!({"type": "text", "text": CC_SYSTEM, "cache_control": crate::cache_breakpoint()}));

        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2, "caller system moves into messages: {:?}", msgs);
        assert_eq!(msgs[0]["role"], json!("user"));
        assert_eq!(msgs[1]["role"], json!("system"));
        assert_eq!(msgs[1]["content"][0]["text"], json!("caller prompt"));
        assert_eq!(msgs[1]["content"][0]["cache_control"], crate::cache_breakpoint());
        let first_user = msgs[0]["content"].as_array().unwrap();
        assert!(
            first_user[0]["text"].as_str().unwrap().starts_with("<system-reminder>"),
            "currentDate reminder leads the first user turn: {:?}",
            first_user
        );
        assert!(first_user[0]["text"].as_str().unwrap().contains("# currentDate"));
        assert_eq!(first_user[1]["text"], json!("x"));
        let uid: Value = serde_json::from_str(out["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(uid["session_id"], json!("sess-1"));
        assert!(uid["device_id"].as_str().unwrap().len() > 10);
        assert!(uid["account_uuid"].as_str().is_some());
    }

    #[test]
    fn billing_header_carries_version_and_entrypoint() {
        let claude = json!({"messages": [{"role": "user", "content": "hello there"}]});
        let h = billing_header(&claude);
        let rest = h.strip_prefix("x-anthropic-billing-header: ").unwrap();
        let (version, tail) = rest.split_once("; ").unwrap();
        let (_, hash) = version.rsplit_once('.').unwrap();
        assert_eq!(version, format!("cc_version={}.{}", CC_VERSION, hash));
        assert_eq!(hash.len(), 3, "build fingerprint must be 3 hex chars: {:?}", hash);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(tail, "cc_entrypoint=cli;");
    }

    #[test]
    fn billing_fingerprint_follows_the_sampled_prompt() {
        let fp = |text: &str| billing_header(&json!({"messages": [{"role": "user", "content": text}]}));
        assert_eq!(fp("hello"), fp("hello"), "same prompt must yield the same fingerprint");
        assert_ne!(fp("hello"), fp("goodbye"));

        let mut units: Vec<u16> = "abcdefghijklmnopqrstuvwxyz".encode_utf16().collect();
        for (i, u) in units.iter_mut().enumerate() {
            if ![4, 7, 20].contains(&i) {
                *u = b'z' as u16;
            }
        }
        units[4] = 'e' as u16;
        units[7] = 'h' as u16;
        units[20] = 'u' as u16;
        assert_eq!(fp(&String::from_utf16(&units).unwrap()), fp("abcdefghijklmnopqrstuvwxyz"));

        units[4] = 'z' as u16;
        assert_ne!(fp(&String::from_utf16(&units).unwrap()), fp("abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn billing_fingerprint_matches_the_reference_implementation() {
        for (prompt, hash) in [
            ("ping", "81d"),
            ("hello", "7e2"),
            ("goodbye", "44b"),
            ("abcdefghijklmnopqrstuvwxyz", "739"),
            ("hi", "81d"),
            ("", "81d"),
            ("a🌍b🎉cd🎉efghij🎉klmnop", "700"),
        ] {
            assert_eq!(
                build_fingerprint(prompt, CC_VERSION),
                hash,
                "prompt {:?}",
                prompt
            );
        }
    }

    #[test]
    fn billing_fingerprint_survives_a_short_prompt() {
        let h = billing_header(&json!({"messages": [{"role": "user", "content": "hi"}]}));
        assert!(h.contains(CC_VERSION), "{}", h);
    }

    #[test]
    fn billing_fingerprint_reads_multibyte_text() {
        let emoji = json!({"messages": [{"role": "user", "content": "a🌍b🎉cd🎉efghij🎉klmnop"}]});
        let h = billing_header(&emoji);
        assert!(h.starts_with("x-anthropic-billing-header: cc_version="), "{}", h);
    }

    #[test]
    fn billing_header_without_any_user_message() {
        let h = billing_header(&json!({"messages": []}));
        assert_eq!(
            h,
            format!(
                "x-anthropic-billing-header: cc_version={}.{}; cc_entrypoint=cli;",
                CC_VERSION,
                build_fingerprint("", CC_VERSION)
            )
        );
    }

    #[test]
    fn config_dir_is_absolute_and_named_after_the_tool() {
        let dir = config_dir();
        assert!(dir.is_absolute(), "config dir must be absolute: {:?}", dir);
        assert!(dir.ends_with("anthropic-proxy"), "{:?}", dir);
        let ctx = Ctx::production();
        assert_eq!(ctx.creds, dir.join(CRED_FILE));
        assert_eq!(ctx.upstream, UPSTREAM);
        assert_eq!(ctx.token_url, oauth::TOKEN_URL);
    }

    #[test]
    fn credentials_are_written_owner_only() {
        let dir = std::env::temp_dir().join(format!("apx-mode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(CRED_FILE);
        write_private(
            &p,
            &json!({"access_token": "secret", "refresh_token": "secret"}),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "got {:o}", mode);
        }
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(back["access_token"], json!("secret"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn device_id_survives_a_refresh() {
        let dir = std::env::temp_dir().join(format!("apx-dev-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(CRED_FILE);
        save_token(&p, &json!({"access_token": "a1", "refresh_token": "r1", "expires_in": 3600}))
            .unwrap();
        let first = stored_device_id(&p);
        save_token(&p, &json!({"access_token": "a2", "refresh_token": "r2", "expires_in": 3600}))
            .unwrap();
        assert_eq!(stored_device_id(&p), first, "device_id must be stable");
        let t = load_token(&p).unwrap();
        assert_eq!(t.access_token, "a2");
        assert_eq!(t.refresh_token, "r2");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_credentials_report_the_path() {
        let p = std::env::temp_dir().join("apx-absent/credentials.json");
        let err = load_token(&p).unwrap_err();
        assert!(err.contains(&p.display().to_string()), "{}", err);
    }

    #[test]
    fn pkce_challenge_is_derived_from_verifier() {
        let url = oauth::authorize_url();
        assert!(url.starts_with("https://claude.ai/oauth/authorize?"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(&format!("client_id={}", oauth::CLIENT_ID)));
        let (verifier, state) = oauth::PENDING.with(|p| p.borrow().clone()).unwrap();
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert!(url.contains(&expected));
        assert!(url.contains(&format!("state={}", state)));
        assert_ne!(verifier, state);
    }

    #[test]
    fn date_is_stamped_without_a_caller_system_prompt() {
        let claude = json!({"model": "m", "messages": [{"role": "user", "content": "x"}]});
        let creds = std::env::temp_dir().join("apx-cloak-nosys/credentials.json");
        let out = cloaked_system(claude, "s", &creds);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0]["content"][0]["text"].as_str().unwrap().contains("# currentDate"));
        assert_eq!(msgs[0]["content"][1]["text"], json!("x"));
    }

    #[test]
    fn caller_metadata_is_not_forwarded() {
        let req = json!({"model": "m", "metadata": {"foo": 1}, "messages": [{"role": "user", "content": "x"}]});
        let out = translate::openai_to_claude(&req).unwrap();
        assert!(out.get("metadata").is_none());
    }

    #[test]
    fn multibyte_image_url_is_rejected_without_panicking() {
        let url = format!("{}{}", "a".repeat(21), "é".repeat(20));
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {"url": url}}]}]
        });
        let e = translate::openai_to_claude(&req).unwrap_err();
        assert!(e.starts_with("unsupported image_url: "), "{}", e);
    }

    #[test]
    fn malformed_tools_are_rejected() {
        let msgs = json!([{"role": "user", "content": "x"}]);
        for tools in [json!([{"type": "function"}]), json!([{"function": {"description": "d"}}]), json!("x")] {
            let req = json!({"model": "m", "tools": tools, "messages": msgs});
            assert!(translate::openai_to_claude(&req).is_err(), "{}", tools);
        }
    }

    #[test]
    fn redact_handles_short_and_multibyte_tokens() {
        assert_eq!(redact(""), "not logged in");
        assert_eq!(redact("abc"), "logged in (token redacted)");
        assert_eq!(redact("abcdefghij"), "logged in (token redacted)");
        assert_eq!(redact("aaaaaé"), "logged in (token redacted)");
        assert_eq!(redact("sk-ant-0123456789abcdefWXYZ"), "logged in as sk-ant…WXYZ");
        assert_eq!(redact("ééééééxxxxxxxxxxxxxxxxééé1"), "logged in as éééééé…ééé1");
    }

    #[test]
    fn credentials_write_is_atomic_and_leaves_no_temp_files() {
        let dir = std::env::temp_dir().join(format!("apx-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("credentials.json");
        save_token(&p, &json!({"access_token": "a", "refresh_token": "r", "expires_in": 10})).unwrap();
        let t = load_token(&p).unwrap();
        assert_eq!(t.access_token, "a");
        assert!(t.expires_at.unwrap() > now());
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("credentials.json")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_credentials_file_is_reported_as_unreadable() {
        let dir = std::env::temp_dir().join(format!("apx-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("credentials.json");
        std::fs::write(&p, "").unwrap();
        let e = load_token(&p).unwrap_err();
        assert!(e.starts_with("unreadable credentials"), "{}", e);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn token_expiry_is_detected() {
        let t = |at| Token { access_token: String::new(), refresh_token: String::new(), expires_at: at };
        assert!(t(Some(0)).is_expiring());
        assert!(t(Some(now() + 10)).is_expiring());
        assert!(!t(Some(now() + 3600)).is_expiring());
        assert!(!t(None).is_expiring());
    }
}
