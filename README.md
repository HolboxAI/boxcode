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

  ▟█▙       ▟█▙    boxcode  v1.9.0
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
  Destructive commands wait for your approval — deleting, force-pushing,
  publishing.

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
  you launched from. **Only destructive actions stop to ask.** Building
  something is dozens of ordinary steps — `mkdir`, `npm install`, `npm run
  build`, a file written for each one — and prompting for every one of those
  never made the dangerous ones safer, it buried them: twenty identical
  prompts get answered `y` by reflex, and so does the twenty-first. So
  deleting, force-pushing, discarding uncommitted work, killing processes,
  uninstalling, running as root, publishing a package and putting anything on
  the internet all still wait for your `y`/`n`, shown in full — and everything
  else just runs. Genuinely catastrophic commands (`rm -rf /`, disk
  formatting, fork bombs, `curl | sh`, ...) are never even offered as a yes/no
  — see `danger.rs`; that tier is deliberately narrow, and covers destruction
  with no way back rather than anything merely unexpected, so writing a log to
  `/tmp` and reading it back works the way it does in any shell. Set
  `approval = "always"` under `[tools]` to be asked
  about every write and command instead. **Upgrading from an earlier version
  loosens your existing install**: the retired `require_approval` key is
  dropped rather than translated, because the app used to write it into every
  config itself, so its presence never meant anyone had chosen it.
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

- **You can see it thinking** — a reasoning model streams its chain of thought
  on a separate field before a word of the answer appears. That field is now
  read: the spinner says `Thinking…` and the latest line of it sits underneath,
  where before there was a blank screen and a frozen counter for minutes at a
  time. The spinner also separates the round in flight from the whole turn
  (`Responding… (12s · 152s this turn)`), because a turn that ran `npm install`
  and four round trips was reporting all of it as time spent responding.
- **The transcript is marks, not emoji** — a tool line that is still running
  wears a spinner and a finished one wears a `·`, so you can see *which* of
  four commands is the slow one rather than only that four are running. No
  pictographs anywhere: they are double-width in some terminals, single in
  others, and replacement boxes without an emoji font — the verb after the mark
  says `read`/`write`/`grep` more precisely than a picture of a page did.
- **Type prompt, Enter to send** — Alt/Shift-Enter for a newline, Esc to
  cancel, ↑/↓ to recall previous prompts, Ctrl-A/E/W/U/K for line editing,
  Ctrl-C twice to exit (the first press arms it, any other key disarms).
  Paste works, and while the session is running your terminal's own scrollback,
  search and selection all work on the transcript. **Quitting then clears it
  off the terminal**, leaving the shell as you found it — the conversation is
  still on disk for `--resume`, and `clear_on_exit = false` under `[ui]` keeps
  it on screen instead.
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
| `/rollback` | Undo every file the model wrote this session (asks first) |

`/provider` and `/model` write to `~/.boxcode/config.toml` and apply
immediately, no restart needed.

`/rollback` undoes files, not history. Every `write_file` and `edit_file`
records what the file held first, and `/rollback` puts those states back —
files the model created are deleted, files it changed are restored to what it
found, and files you edited yourself are never touched. It shows the full list
and waits for a yes. What it cannot undo, it says so about: shell commands
(a build, an `npm install`, an `rm`) change the disk in ways no snapshot
covers, so the confirmation names the commands that ran instead of pretending.
The window opens when boxcode starts, survives `/compact`, and closes on
`/new`.

## Configuration

```toml
[tools]
enabled = true          # false sends no tool schema at all
workspace = "."         # "." = the directory you launched from
approval = "destructive"  # what stops to ask:
                          #   "destructive" (default) — only actions that destroy
                          #     something or put something on the public internet
                          #   "always" — every write and every command, sparing reads

[deploy]
enabled = true            # false removes the deploy_project tool entirely

[update]
check_on_start = true

[ui]
theme = "auto"        # auto | dark | light — auto-detects your terminal, falls back to a palette legible on both
clear_on_exit = true  # wipe the conversation off the terminal when you quit.
                      # Clears the scrollback, not just the visible screen — the
                      # same thing `clear` does, with the same consequence:
                      # anything there before boxcode started goes too. The
                      # session is still on disk for `--resume` either way.
```

Neither `approval` value reaches the two deliberately-uncapped tiers:
destructive-but-legitimate actions always stop for a decision in both, and the
catastrophic tier can't be configured off at all — see `danger.rs` for the
exact rules. Nothing here is a sandbox: these prompts are the only thing
limiting what the model can do with your own permissions, and a looser default
means the destructive tier is now carrying more of that weight on its own.

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
- `src/rollback.rs` — the undo journal behind `/rollback`: what each write
  replaced, and what putting it back means
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
