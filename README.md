# tuisample-code

Terminal UI for Claude Code–style AI coding assistant. Connects to any OpenAI-compatible LLM endpoint.

```
 ◈

  ▟█▙       ▟█▙    tuisample-code  v0.8.0
  ▜███████████▛    a terminal coding assistant
  ██  █████  ██
  ▜███████████▛    Welcome back, you!
    ▜█▛   ▜█▛

  ────────────────────────────────────────────────────────────────

  model     deepseek-chat
  endpoint  https://api.deepseek.com
  cwd       ~/Desktop/HolboxAI/tuisample-code

  /provider switch provider or endpoint
  /model    switch model

  Ask about this project — it can read files and run commands.
  Every command and every write waits for your approval.

╭──────────────────────────────────────────────────────────────────────╮
│❯ add a health check endpoint                                         │
╰──────────────────────────────────────────────────────────────────────╯
  ↵ send  ·  ⌥↵ newline  ·  ↑↓ history  ·  ^c exit
```

While a turn runs, a spinner sits at the end of the transcript — right above
the prompt, where you're already looking:

```
  ❯ add a health check endpoint

  I'll add it to the router and run the tests.
  · $ cargo test — 42 lines

  ⠹ Responding… (4s · ~120 tokens · esc to interrupt)
```


## Quick Start

### 1. Install

macOS / Linux:
```bash
curl -fsSL https://raw.githubusercontent.com/HolboxAI/tuisample-code/main/install.sh | bash
```

Windows (PowerShell):
```powershell
irm https://raw.githubusercontent.com/HolboxAI/tuisample-code/main/install.ps1 | iex
```

Downloads a prebuilt binary for your platform (macOS/Linux/Windows,
x86_64/arm64) from the latest
[release](https://github.com/HolboxAI/tuisample-code/releases) and verifies it
against a published checksum — no Rust toolchain needed, installed in
seconds. Also installs Python's `ddgs` package if it's missing, since
`web_search` needs it.

On macOS/Linux, if your platform has no prebuilt binary yet, `install.sh`
falls back to installing Rust (if missing) and building from source instead,
same as before, just automatically. There is no such fallback on Windows —
building from source there needs the MSVC Build Tools, a much bigger ask than
`rustup` alone, so `install.ps1` will tell you plainly if it can't find a
prebuilt binary rather than trying to set up a C++ toolchain unasked; install
Rust yourself and run `cargo build --release`, or use WSL with the regular
`install.sh`.

Or build from source yourself:
```bash
git clone https://github.com/HolboxAI/tuisample-code
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
internal mirror serving the same `Cargo.toml` and `install.sh`/`install.ps1`
(whichever your platform uses):

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
Created it — printing "Hello, World!" and running it now.
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
╭ Write this file? ────────────────────────────╮
│  📝 hello.py                                  │
│                                              │
│  print("Hello, World!")                      │
│                                              │
│  in /Users/you/project                       │
│                                              │
│  y write  ·  n skip  ·  esc skip             │
╰──────────────────────────────────────────────╯
```

**`y`** does it · **`n`** or **Esc** skips it and tells the model to try
something else. Every action is asked about individually — there is
deliberately no "allow everything from now on" key, so one impatient keystroke
can never cover commands the model has not thought of yet. Reads of a short,
conservative allowlist (`ls`, `cat`, `grep`, `git status`/`diff`, ...) skip the
prompt by default — see `auto_approve_read_only` below.

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

Windows and PowerShell are covered by the same rules: `del /f /s /q C:\`,
`rd /s /q C:\`, `Remove-Item -Recurse -Force C:\`, `format`, `diskpart`,
`cipher /w`, `bcdedit`, `reg delete HKLM`, `Clear-Disk`, and
`vssadmin delete shadows` (which destroys the backups that would let you
recover).

**No setting reaches this.** Not `require_approval = false`, not
`auto_approve_read_only`. There is deliberately no config option to turn it off.

A middle tier — destructive but legitimate — always stops for an explicit
decision, *even with approval switched off entirely*: `rm -rf build`,
`git reset --hard`,
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

## Appearance

The colours adapt to your terminal's background. On launch the app works out
whether you are on a dark or a light terminal — first from `[ui] theme` if you
set it, then from `COLORFGBG`, then by asking the terminal directly (OSC 11).

Most terminals answer, but not all: VS Code, iTerm2 and Apple Terminal set no
`COLORFGBG`, and Windows consoles do not reply to the query. When nothing can
be established, a third palette is used whose colours are legible on a dark
*and* a light background — safe, just less vivid than either tuned one.

If the guess is wrong, or you just want the vivid version, say so outright:

```toml
[ui]
theme = "auto"   # auto | dark | light
```

Every colour is contrast-checked in CI against the background it is for
(4.5:1 for text, 3:1 for rules and borders), so a future palette tweak cannot
quietly make something unreadable again.

## Usage

- **Type prompt** — Bottom input line (paste works too)
- **Enter** — Send prompt
- **Alt-Enter** / **Shift-Enter** — Insert a newline for multi-line prompts
- **Esc** — Cancel ongoing request
- **↑ / ↓** — Recall previous prompts. Inside a multi-line prompt they move
  between its lines first, so a stray ↑ can't swallow what you were writing
- **PgUp / PgDn** — Scroll the transcript
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
- **`/new`** — Forgets the current conversation. The configured provider and
  model are untouched; only the message history and tool-step count reset.
- **`/usage`** — Prints your token usage from `~/.tuisample-code/usage.jsonl`:
  today, the last 7 days, and all time. This is local and per-install only —
  there is no login, so it is the only place this number exists; nothing here
  is ever sent anywhere (see "Anonymous usage pings" below for the one thing
  that is).

`/provider` and `/model` write the result to `~/.tuisample-code/config.toml`
and apply it immediately — no restart needed, even mid-session.

### Anonymous usage pings

There is no login, so there is no way to attribute usage to a person — what
this app can see instead is a random ID generated once per install
(`~/.tuisample-code/device_id`), which labels a machine, not a person. Two
things, and only these two things, ever leave your machine:

- `install.sh`/`install.ps1` sends one `install` ping on a fresh install or an
  `--upgrade`.
- The app itself sends one `active` ping per calendar day (UTC) it's actually
  used, checked against `~/.tuisample-code/last_active` so a long session
  doesn't send more than one.

Each ping carries only `{anon_id, event, version, os, date}` — no prompts, no
file paths, no command text, no conversation content. Both are silent,
best-effort, and never block startup or fail an install: see `src/telemetry.rs`
and the `ping_install`/`Send-InstallPing` functions in `install.sh`/`install.ps1`.
`TUISAMPLE_TELEMETRY_URL=""` disables it on macOS/Linux; PowerShell cannot
represent an explicitly-blank environment variable (`$env:X = ''` deletes it
outright), so use `TUISAMPLE_TELEMETRY_URL=off` on Windows instead.

**The aggregate counts are public**: [tui-telemetry.dhruvm307.workers.dev](https://tui-telemetry.dhruvm307.workers.dev)
shows total installs, distinct anonymous devices seen, and daily-active counts,
live. That page is also the entire ingestion endpoint (see
`telemetry-worker.js` in the repo root) — it's as publicly *writable* as it is
readable, so treat the numbers as self-reported, not verified.

Set `TUISAMPLE_TELEMETRY_URL=""` (explicitly blank, not just unset) before
installing or running the binary to opt out entirely.

This is entirely separate from `/usage` above, which never leaves your
machine at all.

## Architecture

- **Rust + Ratatui** — Terminal UI framework
- **tokio** — Async event loop (handles keyboard + streaming simultaneously)
- **OpenAI-compatible API** — Works with any endpoint (self-hosted, Bedrock, etc.)

Clean, modular structure for easy feature additions:
- `src/main.rs` — Event loop
- `src/app.rs` — State machine
- `src/ui.rs` — Terminal rendering
- `src/theme.rs` — Colours, glyphs, and the spinner, in one place
- `src/llm.rs` — LLM client + streaming
- `src/config.rs` — Configuration loading
- `src/providers.rs` — Built-in provider/model registry for `/provider` and `/model`
- `src/tools.rs` — The model's tools (`run_command`, `read_file`, `write_file`): schemas, execution, timeouts
- `src/workspace.rs` — The working directory commands run in
- `src/usage.rs` — Local per-install token usage log (`/usage`), never transmitted
- `src/telemetry.rs` — Anonymous install/daily-active pings, disabled by default
- `src/dateutil.rs` — Calendar-date helpers shared by the two above
- `telemetry-worker.js` — The Cloudflare Worker that `telemetry.rs`/`install.sh` ping and that serves the public view

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
