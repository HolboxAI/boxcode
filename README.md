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
│ > your prompt here... (Enter to send, Esc cancel)        │
├──────────────────────────────────────────────────────────┤
│ Status: Ready | Press Ctrl-C to exit                    │
└──────────────────────────────────────────────────────────┘
```

## Quick Start

### 1. Install

```bash
curl -fsSL https://raw.githubusercontent.com/HolboxAI/tuisample-code/main/install.sh | bash
```

Or build from source:
```bash
git clone https://github.com/yourcompany/tuisample-code
cd tuisample-code
cargo build --release
./target/release/tuisample-code
```

### 2. Configure

Fastest way: launch `tuisample-code` and type `/provider` — pick a provider from the
list (arrow keys, Enter), then pick a model. If you already have that provider's
conventional API key exported (e.g. `DEEPSEEK_API_KEY` for DeepSeek,
`OPENAI_API_KEY` for OpenAI — pattern is `{PROVIDER}_API_KEY`), it's picked up
automatically; otherwise you're prompted to paste or type it (input hidden). The
choice is written to `~/.tuisample-code/config.toml` so it's remembered next launch.
Not on the list, or pointing at a self-hosted/internal endpoint? Pick
**"Custom endpoint..."** at the bottom of the list instead — you'll be walked
through endpoint, model, and API key manually, same as filling in the file by hand.

Alternatively, skip the picker and set environment variables or write
`~/.tuisample-code/config.toml` directly:

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

### 4. Update

```bash
tuisample-code --upgrade
```

Checks `main` for a newer version and, if there is one, reinstalls in place —
no need to dig out the curl command again. It removes stale copies from other
directories on your `$PATH` and confirms the shell resolves to the new build.

`main` can also carry changes that haven't been given a new version number yet.
To rebuild from the latest source regardless, use `tuisample-code --upgrade
--force`.

> Upgrading from 0.2.0 or earlier? Those builds predate this flag — run the
> install command from step 1 once more, and `--upgrade` works from then on.

Running somewhere with no route to github.com? Point upgrades at a fork or an
internal mirror serving the same `Cargo.toml` and `install.sh`:

```bash
export TUISAMPLE_UPGRADE_URL_BASE=https://git.company.internal/tuisample-code/raw/main
```

## Usage

- **Type prompt** — Bottom input line (paste works too)
- **Enter** — Send prompt
- **Alt-Enter** / **Shift-Enter** — Insert a newline for multi-line prompts
- **Esc** — Cancel ongoing request
- **↑ / ↓ / PgUp / PgDn** — Scroll the transcript
- **Ctrl-A / Ctrl-E** — Jump to start / end of line
- **Ctrl-W** — Delete previous word
- **Ctrl-U / Ctrl-K** — Delete to start / end of line
- **Ctrl-C** — Exit

`endpoint` may be given as `https://host`, `https://host/v1`, or the full
`https://host/v1/chat/completions` — all three resolve correctly. Environment
variables override values in `config.toml`.

### Slash Commands

- **`/provider`** — Opens a picker (↑/↓ to navigate, Enter to select, Esc to
  cancel) of built-in providers, plus a **"Custom endpoint..."** entry that
  preserves the "any OpenAI-compatible endpoint" support above — it's not
  limited to the built-in list. Selecting a provider chains straight into a
  model picker for it.
- **`/model`** — Re-picks just the model for whichever provider is currently
  configured, without going through `/provider` again. If no provider has been
  set yet (e.g. you're only using `TUISAMPLE_*` env vars or a custom endpoint),
  this shows an inline error telling you to run `/provider` first.

Both write the result to `~/.tuisample-code/config.toml` and apply it
immediately — no restart needed, even mid-session.

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
- `src/providers.rs` — Built-in provider/model registry for `/provider` and `/model`

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
