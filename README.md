# tuisample-code

Terminal UI for Claude Code–style AI coding assistant. Connects to any OpenAI-compatible LLM endpoint.

```
┌──────────────────────────────────────────────────────────┐
│ tuisample-code | llm.company.internal | model: company-70b │
├──────────────────────────────────────────────────────────┤
│                                                           │
│ Assistant: Here's the function...                        │
│ def hello_world():                                       │
│   return "Hello, World!"                                │
│                                                           │
│ ✓ Generated 120 tokens in 1.2s                          │
│                                                           │
│ You: > write a hello world function                     │
│                                                           │
├──────────────────────────────────────────────────────────┤
│ > your prompt here... (Ctrl-Enter to send, Esc cancel)  │
├──────────────────────────────────────────────────────────┤
│ Status: Ready | Press Ctrl-C to exit                    │
└──────────────────────────────────────────────────────────┘
```

## Quick Start

### 1. Install

```bash
curl -fsSL https://github.com/yourcompany/tuisample-code/releases/download/latest/install.sh | bash
```

Or build from source:
```bash
git clone https://github.com/yourcompany/tuisample-code
cd tuisample-code
cargo build --release
./target/release/tuisample-code
```

### 2. Configure

Set environment variables or create `~/.tuisample-code/config.toml`:

```toml
[llm]
endpoint = "https://llm.company.internal:8443"
model = "company-llm-70b-v1.2"
api_key = "sk_company_xxx"
```

Or use environment variables:
```bash
export TUISAMPLE_ENDPOINT=https://llm.company.internal:8443
export TUISAMPLE_MODEL=company-llm-70b-v1.2
export TUISAMPLE_API_KEY=sk_company_xxx
```

### 3. Run

```bash
tuisample-code
```

## Usage

- **Type prompt** — Bottom input line
- **Ctrl-Enter** — Send prompt
- **Esc** — Cancel ongoing request
- **Ctrl-C** — Exit

## Architecture

- **Rust + Ratatui** — Terminal UI framework
- **tokio** — Async event loop (handles keyboard + streaming simultaneously)
- **OpenAI-compatible API** — Works with any endpoint (self-hosted, Bedrock, etc.)

Clean, modular structure for easy feature additions:
- `src/main.rs` — Event loop
- `src/app.rs` — State machine
- `src/ui.rs` — Terminal rendering
- `src/llm.rs` — LLM client + streaming
- `src/config.rs` — Configuration loading

## What's Next

Day 2+:
- Code generation command (separate flow)
- File context collection
- GitHub integration (VPC-only)
- Test generation

## Development

```bash
# Build
cargo build --release

# Run with debug logging
RUST_LOG=debug tuisample-code

# Test (if you add tests)
cargo test
```

## License

MIT
