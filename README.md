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

## Running commands, and reading/writing files

The model has three tools in the directory you launched from, so you can ask
about the actual project instead of pasting code in, and have it create or
change files directly instead of hand-encoding writes into shell commands:

```
> create hello.py and run it
📝 /Users/you/project/hello.py
Assistant: Created it — printing "Hello, World!" and running it now.
$ python3 hello.py — 1 line
```

`read_file`/`write_file` handle reading and creating/overwriting a single
file. `run_command` is for everything else — search (`grep`), builds, tests,
running a program, listing an archive (`unzip -l`), extracting a PDF
(`pdftotext`) — anything installed on your machine.

### You approve every write and every command

Each one stops and waits for you — a write shows the file's full new content,
not a shell string:

```
┌ Write this file? ────────────────────────────┐
│ 📝 hello.py                                   │
│                                              │
│ print("Hello, World!")                       │
│                                              │
│ in /Users/you/project                        │
│                                              │
│ y write   n skip   a run everything this session│
└──────────────────────────────────────────────┘
```

**`y`** does it · **`n`** or **Esc** skips it and tells the model to try
something else · **`a`** stops asking for the rest of the session. Reads of a
short, conservative allowlist (`ls`, `cat`, `grep`, `git status`/`diff`, ...)
skip the prompt by default — see `auto_approve_read_only` below.

These prompts are the *only* thing limiting what the model can do. `run_command`
can read any file your user can read, write anywhere, and delete anything —
there is no sandbox, and there is no honest way to build one by inspecting
command strings. `write_file`/`read_file` are checked against the project
directory before anything happens (see `tools::resolve_in_workspace`), which
a raw shell command cannot offer, but that is a guardrail against typos and
injected paths, not a sandbox either. Read each prompt before pressing `y`.

Commands run with **stdin closed** and are killed after a timeout, so anything
interactive (`vim`, a dev server, a REPL) will time out rather than hang.

### Some things are refused outright

A third tier sits above the prompt. Genuinely catastrophic commands are never
run and are **never even offered for approval** — offering `rm -rf /` as a y/n
question is itself the bug, since one mistyped keystroke accepts it and there
is no undo:

```
⛔ $ rm -rf / — blocked
   `rm` aimed at `/`, which is outside the project directory
```

Refused: deleting anything outside the project directory or the project itself
(`rm -rf /`, `~`, `/etc`, `../..`, `.`, `*`), `--no-preserve-root`, disk
formatting (`mkfs`, `fdisk`, `dd of=/dev/sda`), writing to raw devices, fork
bombs, shutdown/reboot, piping a download into a shell (`curl … | sh`),
executing base64-decoded data, `kill -9 1`, and recursive `chmod`/`chown` on
system paths. Every segment of a chained command is checked, so
`ls && rm -rf /` is caught too.

**No setting or keypress reaches this.** Not `a`, not
`require_approval = false`, not `auto_approve_read_only`. There is deliberately
no config option to turn it off.

A middle tier — destructive but legitimate — always stops for an explicit
decision, *even after you press `a`*: `rm -rf build`, `git reset --hard`,
`git clean -fd`, force-push, `sudo` anything, `find … -delete`, uninstalls,
`docker prune`. The prompt shows a red **DESTRUCTIVE** banner and why.

> This is not a sandbox, and no blocklist can be one. A command that builds its
> argument at runtime (`rm -rf $(printf '\x2f')`) defeats any static check —
> such commands are forced to the always-ask tier rather than judged safe, but
> the honest claim is narrow: this catches destructive commands a model
> produces **by mistake**, which is the realistic failure mode. It does not
> stop a determined attacker. Real containment needs an OS sandbox.

### Configuration

```toml
[tools]
enabled = true                # false sends no tool schema at all
workspace = "."                # "." = the directory you launched from
require_approval = true        # false = the model runs commands unattended
auto_approve_read_only = true  # skip the prompt for a narrow read-only allowlist
command_timeout_secs = 60
max_output_bytes = 65536  # ceiling on one command's output
max_steps = 10            # command rounds per prompt before the model must answer
```

Per-run: `TUISAMPLE_WORKSPACE=/path/to/project`, `TUISAMPLE_TOOLS_ENABLED=0`.

> **`require_approval = false` hands the model an unattended shell** on your
> machine. It exists for scripted testing. If you set it, the welcome screen
> says `UNATTENDED` in red every launch.

`auto_approve_read_only` skips the popup only for a short, conservative
allowlist of commands that cannot change anything on disk -- `ls`, `cat`,
`grep`, `git status`/`diff`/`log`/`show`, and similar (see
`tools::is_read_only`). Anything chained with `;`, `|`, `&&`, `>`, or a
subshell falls back to asking, even if it starts with one of those. Everything
else -- writes, deletes, `git push`, `find -delete`, arbitrary other commands
-- still stops for a decision regardless of this setting. Set it to `false` to
go back to asking about every command, including reads.

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
- `src/tools.rs` — The model's tools (`run_command`, `read_file`, `write_file`): schemas, execution, timeouts
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
