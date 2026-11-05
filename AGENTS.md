# AGENTS.md

Local OpenAI-compatible proxy in front of Anthropic's `/v1/messages`, authenticated with a Claude
Code OAuth subscription token. Single account, no config file, no management API.

## Commands

- `cargo test` — unit tests in `src/lib.rs` plus offline e2e tests in `tests/e2e.rs` (they spin up a
  mock upstream and temp credentials on localhost; no network or OAuth needed).
- `cargo test --lib <name>` / `cargo test --test e2e <name>` — run one test.
- `cargo build --release` — requires `cmake` and a C/C++ toolchain (BoringSSL via `wreq`/`btls-sys`).
- `cargo run -- login|status|serve` — `serve` is the default when no subcommand is given.
- No CI, lint, or formatter config exists; `cargo test` is the verification gate.

## Layout

- `src/lib.rs` — CLI parsing (`run`), credential storage, Claude Code request cloaking, most unit tests.
- `src/server.rs` — axum handler, auth/header injection, SSE re-streaming, 401 refresh-and-retry.
- `src/translate.rs` — OpenAI ⇄ Claude request/response translation.
- `src/oauth.rs` — PKCE login callback and token refresh.
- `src/main.rs` — thin wrapper over `anthropic_proxy::run`.
- `tests/e2e.rs` — end-to-end tests; build the router directly with a custom `Ctx` (creds path,
  upstream URL, token URL) and bind an ephemeral port. `Ctx::production()` is only for the real CLI.
  Extend tests through this seam rather than adding global config or env-var hooks.

Listening addresses are fixed: API `127.0.0.1:31701`, OAuth callback `127.0.0.1:54545`.

## Claude Code cloaking (intentional; do not "clean up")

Upstream requires the request to look like the real Claude Code CLI:

- `cloaked_system()` injects a billing header whose fingerprint is derived from the first user
  message and `CC_VERSION`, prepends the Claude Code system prompt with a 1h cache breakpoint,
  moves a caller `system` prompt into a `role: "system"` message after the first user turn, stamps a
  current-date reminder, and replaces `metadata` with a device/session `user_id`.
- `server::send` sends an exact CLI header set: OAuth `Bearer` only (never `x-api-key`),
  `anthropic-beta`, stainless, and user-agent values. `CC_VERSION`, `SDK_VERSION`, `NODE_VERSION`,
  and `CC_USER_AGENT` are pinned to a real Claude Code release; keep them mutually consistent.
- `wreq` with `wreq_util::Emulation::Chrome136` is deliberate TLS/HTTP fingerprint emulation. Do not
  replace it with a plain reqwest client.

## Token handling

- Credentials are written atomically with mode `0600` under the platform config dir; tests pass
  their own temp `Ctx.creds`.
- Refresh is serialized by a process-wide Tokio mutex: Anthropic rotates refresh tokens, so parallel
  refreshes invalidate each other. `refresh_token(ctx, rejected)` re-checks the token after taking
  the lock. Keep that check when editing `send()`'s 401 retry.
- `oauth::PENDING` (PKCE verifier/state) is `thread_local`; `authorize_url`, `callback_server`, and
  `exchange_code` must run on the same thread, which is why `login` uses a current-thread runtime.

## Behaviour contracts pinned by tests

- Truncated/invalid upstream JSON → 502 error body, never an empty or partial completion.
- A truncated SSE stream emits an error chunk, never a fake `finish_reason: "stop"`.
- Non-2xx upstream responses are relayed verbatim; streaming requests still get JSON errors, not SSE.
- `mcp_`-prefixed tool names are case-mapped on the way out and restored on the way back.
- Consecutive same-role messages are merged; caller-supplied `metadata` is discarded.
- Images must be base64 `data:` URLs; other `image_url` forms are rejected with 400.
