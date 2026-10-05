use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const SCOPE: &str = "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const REDIRECT_URI: &str = "http://localhost:54545/callback";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

fn random_string(alphabet: &[u8], len: usize) -> String {
    let mut rng = rand::rng();
    (0..len)
        .map(|_| char::from(alphabet[rng.random_range(0..alphabet.len())]))
        .collect()
}

pub fn authorize_url() -> String {
    let verifier = random_string(
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~",
        64,
    );
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_string(
        b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
        32,
    );

    let query = [
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPE),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
    ]
    .iter()
    .map(|(k, v)| format!("{}={}", k, urlencode(v)))
    .collect::<Vec<_>>()
    .join("&");

    PENDING.with(|p| *p.borrow_mut() = Some((verifier, state)));
    format!("{}?{}", AUTHORIZE_URL, query)
}

thread_local! {
    pub static PENDING: std::cell::RefCell<Option<(String, String)>> = const { std::cell::RefCell::new(None) };
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

const CALLBACK_ADDR: &str = "127.0.0.1:54545";

pub struct Callback {
    pub code: tokio::sync::oneshot::Receiver<String>,
    pub server: tokio::task::JoinHandle<()>,
}

type CodeSender = Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<String>>>>;

pub async fn callback_server(expected_state: String) -> Result<Callback, String> {
    use axum::extract::{Query, State};
    use axum::http::StatusCode;
    use std::collections::HashMap;

    let (tx, rx) = tokio::sync::oneshot::channel();
    let sender: CodeSender = Arc::new(tokio::sync::Mutex::new(Some(tx)));
    let expected_state = Arc::new(expected_state);

    let app = axum::Router::new()
        .route(
            "/callback",
            axum::routing::get(
                |State((sender, expected)): State<(CodeSender, Arc<String>)>,
                 Query(q): Query<HashMap<String, String>>| async move {
                    let code = q.get("code").cloned().unwrap_or_default();
                    if code.is_empty() {
                        let err = q.get("error").cloned().unwrap_or_default();
                        return (StatusCode::BAD_REQUEST, format!("error: {}", err));
                    }
                    if q.get("state").map(String::as_str) != Some(expected.as_str()) {
                        return (StatusCode::BAD_REQUEST, "error: state mismatch".to_string());
                    }
                    if let Some(tx) = sender.lock().await.take() {
                        tx.send(code).ok();
                    }
                    (StatusCode::OK, "OK".to_string())
                },
            ),
        )
        .with_state((sender, expected_state));

    let listener = tokio::net::TcpListener::bind(CALLBACK_ADDR)
        .await
        .map_err(|e| format!("cannot listen on {} for the OAuth callback: {}", CALLBACK_ADDR, e))?;
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    Ok(Callback { code: rx, server })
}

pub async fn exchange_code(ctx: &crate::Ctx, code: &str) -> Result<Value, String> {
    let (verifier, state) = PENDING.with(|p| p.borrow().clone()).unwrap_or_default();
    post_token(
        &ctx.token_url,
        json!({
            "grant_type": "authorization_code",
            "code": code,
            "redirect_uri": REDIRECT_URI,
            "client_id": CLIENT_ID,
            "code_verifier": verifier,
            "state": state,
        }),
    )
    .await
}

pub async fn refresh(token_url: &str, refresh_token: &str) -> Result<Value, String> {
    post_token(
        token_url,
        json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "scope": SCOPE,
        }),
    )
    .await
}

async fn post_token(url: &str, body: Value) -> Result<Value, String> {
    let resp = crate::http_client()
        .post(url)
        .header("content-type", "application/json")
        .header("user-agent", crate::CC_USER_AGENT)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("token endpoint {}: {}", status, text));
    }
    serde_json::from_str(&text).map_err(|e| e.to_string())
}