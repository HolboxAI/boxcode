# tuisample-code

Agentic coding assistant in your terminal, on any OpenAI-compatible LLM endpoint.

It doesn't just answer — it reads your files, searches the repo, edits code and runs
your build. Launch it from the root of the project you want it to work on; that
directory is its workspace, and it cannot read or write outside it.

```
┌──────────────────────────────────────────────────────────────┐
│ tuisample-code | llm.company.internal | model: company-70b   │
├──────────────────────────────────────────────────────────────┤
│ You: add a --json flag to the CLI and a test for it          │
│                                                              │
│ Coder: Let me see how flags are parsed today.                │
│ ● grep(--\w+ in src/main.rs)                                 │
│   │ src/main.rs:41:            "-V" | "--version" => {       │
│ ● read_file(src/main.rs)                                     │
│   │      1  mod agent;                                       │
│   │      2  mod app;                                         │
│   │ … 224 more lines                                         │
│ ● edit_file(src/main.rs)                                     │
│   │ Replaced 1 occurrence in src/main.rs.                    │
│ ● run_shell(cargo test --all)                                │
│   │ test result: ok. 156 passed; 0 failed                    │
│                                                              │
│ Coder: Added --json to the flag match and a test covering    │
│ it. cargo test --all passes.                                 │
├──────────────────────────────────────────────────────────────┤
│ What should I change? (Enter to send, Ctrl-C to exit)        │
├──────────────────────────────────────────────────────────────┤
│ Status: Ready | Enter send · Esc cancel · /new reset         │
└──────────────────────────────────────────────────────────────┘
```

Reads and searches run on their own. Before it writes a file or runs a command,
it asks:

```
┌ Approve action ──────────────────────────────────┐
│ The agent wants to:                              │
│ run_shell(cargo test --all)                      │
│                                                  │
│ [a] allow once   [d] deny                        │
│ [s] allow every `cargo` command for this session │
│ Esc cancels the whole run.                       │
└──────────────────────────────────────────────────┘
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

Agent behaviour can be tuned in the same file. All three are optional:

```toml
[agent]
max_iterations = 25      # tool rounds one prompt may take before giving up
shell_timeout_secs = 120 # per command, capped at 600
max_tokens = 8192        # per turn; coding turns carry whole files
```

### 3. Run

```bash
cd ~/code/my-project     # this becomes the workspace
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

Ask for a change, not just an answer:

- *"add a `--json` flag to the CLI and a test for it"*
- *"why does the auth test fail on CI but not locally?"*
- *"this function is doing three things — split it up"*

### Keys

- **Type prompt** — Bottom input line (paste works too)
- **Enter** — Send prompt
- **Alt-Enter** / **Shift-Enter** — Insert a newline for multi-line prompts
- **Esc** — Cancel the run
- **↑ / ↓ / PgUp / PgDn** — Scroll the transcript
- **Ctrl-A / Ctrl-E** — Jump to start / end of line
- **Ctrl-W** — Delete previous word
- **Ctrl-U / Ctrl-K** — Delete to start / end of line
- **Ctrl-C** — Exit

### At an approval prompt

- **a** — allow once
- **s** — allow for the rest of the session (only offered when it's safe to
  generalise; see below)
- **d** — deny. The agent is told, and adapts rather than failing.
- **Esc** — deny *and* cancel the whole run

## What the agent can do

| Tool | Approval |
|---|---|
| `read_file`, `list_dir`, `glob`, `grep` | Runs on its own |
| `write_file`, `edit_file` | Asks |
| `run_shell` — builds, tests, `git`, `gh` | Asks |

Two limits are enforced regardless of what the model asks for:

- **Nothing outside the workspace.** Paths are resolved through symlinks before
  the check, so neither `../../etc/passwd` nor a symlink pointing out of the tree
  gets through.
- **Session grants are scoped.** Allowing `cargo test` for the session covers
  `cargo build` too, but not `rm`. A command combining several programs
  (`cd x && rm -rf /`) can't be granted for a session at all — it's asked every
  time, because the first word doesn't tell you what it does.

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
- **`/new`** — Forgets the conversation and starts fresh, without restarting the
  process. The transcript stays on screen; the agent just stops carrying it.

`/provider` and `/model` write the result to `~/.tuisample-code/config.toml` and
apply it immediately — no restart needed, even mid-session.

## Requirements

The endpoint must support OpenAI-style function calling (a `tools` array and
`tool_calls` in the response). DeepSeek and OpenAI both do, as do most current
self-hosted servers (vLLM, llama.cpp, Ollama, TGI). Without it the model can
still talk, but it can't touch your code.

## Architecture

- **Rust + Ratatui** — Terminal UI framework
- **tokio** — Async event loop (keyboard, streaming and tool execution at once)
- **OpenAI-compatible API** — Works with any endpoint (self-hosted, Bedrock, etc.)

```
prompt ─▶ agent loop ─▶ model asks for tools ─▶ permission gate ─▶ tools run
             ▲                                                        │
             └──────────────── results fed back ──────────────────────┘
```

- `src/main.rs` — Event loop; starts a run, routes agent events to the UI
- `src/agent/` — Agent registry and system prompts (`mod.rs`), the loop (`run.rs`)
- `src/tools/` — What the model can do: `fs.rs`, `search.rs`, `shell.rs`
- `src/permission.rs` — What runs unattended, and how session grants are scoped
- `src/app.rs` — UI state: the timeline, overlays, input editing
- `src/ui.rs` — Terminal rendering
- `src/llm.rs` — Streaming client, tool-call protocol
- `src/config.rs` — Configuration loading
- `src/providers.rs` — Built-in provider/model registry for `/provider` and `/model`

## What's Next

The agent registry in `src/agent/mod.rs` currently holds one general-purpose
`coder`. It's shaped for several: the next step splits it into specialists —
planner, investigator, coder, fixer, test-writer, and a merge/CI integrator —
with a lead agent that picks who to hand each piece to via a `delegate` tool.
Each specialist gets its own system prompt and its own subset of the tool
registry; the loop and the permission gate stay exactly as they are.

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
