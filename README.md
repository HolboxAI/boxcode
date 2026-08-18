# boxcode

A coding assistant that lives in your terminal. It reads your files and runs
your commands — and waits for you before every one. Connects to any
OpenAI-compatible LLM endpoint.

**[boxcode.sh](https://boxcode.sh)**

> ### Renamed in v1.0.0
>
> `tuisample-code` is now **`boxcode`**. Reinstall with the command below —
> `~/.tuisample-code/` and `TUISAMPLE_*` env vars still migrate/work
> automatically, but are deprecated in favor of `~/.boxcode/`/`BOXCODE_*`.

```
 ◈

  ▟█▙       ▟█▙    boxcode  v1.7.0
  ▜███████████▛    a terminal coding assistant
  ██  █████  ██
  ▜███████████▛    Welcome back, you!
    ▜█▛   ▜█▛

  ────────────────────────────────────────────────────────────────

  model     deepseek-chat
  endpoint  https://api.deepseek.com
  cwd       ~/Desktop/HolboxAI/boxcode

  /provider switch provider or endpoint
  /model    switch model

  Ask about this project — it can read files and run commands.
  Every command and every write waits for your approval.

╭──────────────────────────────────────────────────────────────────────╮
│❯ add a health check endpoint                                         │
╰──────────────────────────────────────────────────────────────────────╯
  ↵ send  ·  ⌥↵ newline  ·  ↑↓ history  ·  ^c exit
```

## Quick Start

**Install** — macOS/Linux: `curl -fsSL https://boxcode.sh/install.sh | bash`
Windows (PowerShell): `irm https://boxcode.sh/install.ps1 | iex`

Downloads a prebuilt binary (no Rust toolchain needed) and verifies it against
a published checksum. No prebuilt for your platform? It falls back to
installing Rust and building from source (macOS/Linux only — on Windows,
install Rust yourself and run `cargo build --release`, or use WSL). Or build
from source directly:

```bash
git clone https://github.com/HolboxAI/boxcode && cd boxcode
cargo build --release && ./target/release/boxcode
```

**Configure** — launch `boxcode` and run `/provider`: pick a provider, then a
model. An existing `{PROVIDER}_API_KEY` env var (e.g. `DEEPSEEK_API_KEY`) is
picked up automatically; otherwise you're prompted for it. Not on the list?
Pick **"Custom endpoint..."** for any OpenAI-compatible endpoint. Or skip the
picker and edit `~/.boxcode/config.toml` / set `BOXCODE_ENDPOINT`,
`BOXCODE_MODEL`, `BOXCODE_API_KEY` directly.

**Run** — `boxcode`

**Update** — it checks once a day on startup and asks before installing
anything (default answer is no). Reinstall by hand any time with
`boxcode --upgrade` (always a force-install, so it also fixes a broken
install). Turn the automatic check off with `BOXCODE_NO_UPDATE_CHECK=1` or
`check_on_start = false` under `[update]`.

## What it does

Everything below happens because you **asked in plain English** — there's no
command for most of these, on purpose: a sentence carries the intent and the
details in one message, where a flag-heavy command would need several screens
to ask for the same thing.

- **Reads, writes and edits your files, and runs commands** in the directory
  you launched from — every write and every command waits for your `y`/`n`
  first, shown in full (a file's real new content, not a shell string).
  Genuinely catastrophic commands (`rm -rf /`, disk formatting, fork bombs,
  `curl | sh`, ...) are never even offered as a yes/no — see `danger.rs`.
- **Every file change is a diff, before and after** — a write or an edit is
  approved by looking at red `-` and green `+` lines with real line numbers,
  against the file as it is on disk right now, not by reading a whole new file
  or a "replace this / with this" pair with no idea where it lands. The
  approved change then leaves that same diff in the transcript, so "wrote 4kb"
  becomes "changed these four lines". The preview is produced by the very code
  that applies the edit, so what you approve and what happens cannot differ.
- **`/plan`** — research first, get an editable plan, *then* approve the
  approach before any file changes. The plan lands as `plan.md` in your
  project, so it's a file you own, not hidden state. Full details:
  [docs/plan-mode.md](docs/plan-mode.md).
- **Publishes a live URL, with real auth and a real database, in the same
  conversation** — say "publish this" and get a shareable link; say "add
  sign-up and sign-in" and get real email/password auth (hosted, no account
  needed); say "add a database" and the model can run SQL against a real
  per-project SQLite file, scoped to the signed-in user on request. The
  published page also **live-reloads** — a tab already open picks up your next
  publish automatically. A visitor can leave a change request from the live
  page itself (e.g. from their phone) that your next session picks up.
  `/pull` switches between any project this machine has published before.
- **Deploys to Vercel or Netlify** — say "deploy this to vercel" and it
  detects your framework, builds, and hands back a live URL (or the build
  log, to fix and retry) — all in one approval-gated tool call. Full details:
  [docs/deploying.md](docs/deploying.md).
- **`BOXCODE.md`** (or `AGENTS.md`) at your project root rides along with
  every request — run `/init` to have the model write one. **Sessions**
  persist to `~/.boxcode/sessions/` per directory; `--resume`/`/resume` picks
  up where you left off, even after a crash. **`/compact`** summarizes a long
  conversation (automatically, past a token threshold, or on demand) instead
  of losing it with `/new`.

## Usage

- **Type prompt, Enter to send** — Alt/Shift-Enter for a newline, Esc to
  cancel, ↑/↓ to recall previous prompts, Ctrl-A/E/W/U/K for line editing,
  Ctrl-C to exit. Paste works; your terminal's own scrollback, search and
  selection all work on the transcript too.
- `endpoint` accepts `https://host`, `https://host/v1`, or the full chat-
  completions URL — all three resolve. Env vars override `config.toml`.

### Slash commands

| Command | Does |
| --- | --- |
| `/plan` | Toggle plan mode (research → approve a plan → then it can write) |
| `/provider` | Pick a provider, then chains into a model picker |
| `/model` | Re-pick just the model for the current provider |
| `/init` | Explore the project and write/update `BOXCODE.md` |
| `/resume` | Reload this directory's last session into a fresh conversation |
| `/pull` | Switch to a different project this machine has published |
| `/new` | Start a fresh conversation (old one stays on disk for `/resume`) |
| `/compact` | Summarize the conversation now, freeing context |
| `/usage` | Local token usage — today, last 7 days, all time |

`/provider` and `/model` write to `~/.boxcode/config.toml` and apply
immediately, no restart needed.

## Configuration

```toml
[tools]
enabled = true                # false sends no tool schema at all
workspace = "."                # "." = the directory you launched from
require_approval = true        # false = the model runs commands unattended (UNATTENDED banner shown)
auto_approve_read_only = true  # skip the prompt for a narrow read-only allowlist (ls, cat, grep, git status/diff/log)

[deploy]
enabled = true            # false removes the deploy_project tool entirely

[update]
check_on_start = true

[ui]
theme = "auto"   # auto | dark | light — auto-detects your terminal, falls back to a palette legible on both
```

Everything above the two deliberately-uncapped tiers (destructive-but-legit
actions always stop for a decision, even with `require_approval = false`; the
catastrophic tier can't be configured off at all) — see `danger.rs` for the
exact rules. Nothing here is a sandbox: these prompts are the only thing
limiting what the model can do with your own permissions.

## Privacy

No login, no way to attribute usage to a person. Two anonymous pings only
(`{anon_id, event, version, os, date}`, no prompts/paths/content): one on
install, one per active day. `/usage` (local-only, never sent) is separate.
Opt out entirely with `BOXCODE_TELEMETRY_URL=""` (or `=off` on Windows) before
installing. Aggregate counts are public: see `src/telemetry.rs`.

## Architecture

Rust + Ratatui (terminal UI) + tokio (async), talking to any OpenAI-compatible
chat-completions endpoint.

- `src/app.rs` — state machine · `src/ui.rs` — rendering · `src/llm.rs` — the
  streaming client
- `src/tools.rs` — the model's tools: schemas, execution, timeouts
- `src/config.rs`, `src/providers.rs` — configuration and the built-in
  provider/model registry
- `src/session.rs`, `src/usage.rs`, `src/telemetry.rs` — local session log,
  local usage log, anonymous pings (all separate, see Privacy above)
- `src/plan.rs` — `plan.md`'s format and progress tracking
- `src/diff.rs` — the line diff behind every file-change preview and transcript
  entry
- `src/artifacts.rs`, `src/auth.rs`, `src/db.rs`, `src/requests.rs` —
  publishing, hosted auth, hosted per-project SQLite, the change-request
  mailbox; `infra/` holds the control-planes these talk to
- `src/deploy/` — one file per hosting provider behind a shared
  `DeploymentProvider` trait (see [docs/deploying.md](docs/deploying.md) for
  how to add one)

## What's Next

- A diff preview when a *command* (not a write) is about to modify tracked files
- Remembering per-command approvals across a session
- GitHub integration (VPC-only)
- Test generation
- More deployment providers — AWS Amplify, Cloudflare Pages, GitHub Pages, Render

## Development

```bash
cargo build --release        # build
RUST_LOG=debug boxcode       # run with debug logging
cargo test                   # test
```

## License

MIT
