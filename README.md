# Anthropic Proxy

A local proxy that exposes an OpenAI-compatible `POST /v1/chat/completions` endpoint backed by a
Claude Code OAuth subscription token. Single account, no config file, no management API.

## Build

```
cargo build --release
```

Requires `cmake` and a C/C++ toolchain: the TLS stack is BoringSSL (`btls-sys`), which is
what makes the handshake look like a browser instead of a Rust HTTP client.

## Use

```
anthropic-proxy login    # prints an authorize URL, then waits on 127.0.0.1:54545
anthropic-proxy status   # prints a redacted token prefix
anthropic-proxy serve    # listens on 127.0.0.1:31701
```

## Run at login (macOS)

`serve` runs in the foreground. To keep it up across reboots, save this as
`~/Library/LaunchAgents/local.anthropic-proxy.plist`, replacing `$PWD` with the repo root:

```
<plist version="1.0">
<dict>
  <key>Label</key><string>local.anthropic-proxy</string>
  <key>ProgramArguments</key>
  <array><string>$PWD/target/release/anthropic-proxy</string><string>serve</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
</dict>
</plist>
```

Then load it:

```
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/local.anthropic-proxy.plist
```

## Where credentials live

`login` writes `credentials.json` mode `0600` to the platform config directory:

| Platform | Path |
|---|---|
| macOS | `~/Library/Application Support/anthropic-proxy/credentials.json` |
| Linux | `~/.config/anthropic-proxy/credentials.json` |