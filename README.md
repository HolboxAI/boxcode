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

## Running commands

The model can run shell commands in the directory you launched from, so you can
ask about the actual project instead of pasting code in:

```
> what does the event loop in main.rs do?
$ sed -n '90,150p' src/main.rs — 61 lines
Assistant: It polls for terminal input every 16ms, drains …
```

Reading a file is `cat`, searching is `grep`, listing an archive is `unzip -l`,
extracting text from a PDF is `pdftotext` — anything installed on your machine.

### You approve every command

Each command stops and waits for you:

```
┌ Run this command? ───────────────────────────┐
│ check what the tests cover                   │
│                                              │
│ $ grep -rn "fn test" tests/                  │
│                                              │
│ in /Users/you/project                        │
│                                              │
│ y run   n skip   a run everything this session│
└──────────────────────────────────────────────┘
```

**`y`** runs it · **`n`** or **Esc** skips it and tells the model to try
something else · **`a`** stops asking for the rest of the session.

This prompt is the *only* thing limiting what the model can do. A shell command
can read any file your user can read, write anywhere, and delete anything —
there is no sandbox, and there is no honest way to build one by inspecting
command strings. Read each command before pressing `y`.

Commands run with **stdin closed** and are killed after a timeout, so anything
interactive (`vim`, a dev server, a REPL) will time out rather than hang.

### Configuration

```toml
[tools]
enabled = true            # false sends no tool schema at all
workspace = "."           # "." = the directory you launched from
require_approval = true   # false = the model runs commands unattended
command_timeout_secs = 60
max_output_bytes = 65536  # ceiling on one command's output
max_steps = 10            # command rounds per prompt before the model must answer
```

Per-run: `TUISAMPLE_WORKSPACE=/path/to/project`, `TUISAMPLE_TOOLS_ENABLED=0`.

> **`require_approval = false` hands the model an unattended shell** on your
> machine. It exists for scripted testing. If you set it, the welcome screen
> says `UNATTENDED` in red every launch.

Works on macOS, Linux, and Windows — commands run through `sh -c`, or `cmd /C`
on Windows, and the model is told which platform it is on so it reaches for
`dir`/`type`/`findstr` rather than `ls`/`cat`/`grep`.

> Your endpoint needs to support OpenAI-style tool calling. If it doesn't, the
> request comes back as `HTTP 400` — set `enabled = false` under `[tools]` and
> everything else keeps working as before.

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
- `src/tools.rs` — The `run_command` tool: schema, execution, timeouts
- `src/workspace.rs` — The working directory commands run in

## What's Next

- A diff preview when a command is about to modify tracked files
- Remembering per-command approvals across a session
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
