# boxcode

A coding assistant that lives in your terminal. It reads your files and runs
your commands — and waits for you before every one. Connects to any
OpenAI-compatible LLM endpoint.

**[boxcode.sh](https://boxcode.sh)**

> ### Renamed in v1.0.0
>
> `tuisample-code` is now **`boxcode`**. If you are upgrading:
>
> ```bash
> # 1. Point your clone at the new repo
> git remote set-url origin https://github.com/HolboxAI/boxcode.git
>
> # 2. Reinstall (this also removes the old binary from your PATH)
> curl -fsSL https://raw.githubusercontent.com/HolboxAI/boxcode/main/install.sh | bash
>
> # 3. If anything is left behind
> sudo rm -f /usr/local/bin/tuisample-code
> ```
>
> **The binary is `boxcode`.** `~/.tuisample-code/` moves to `~/.boxcode/`
> automatically on first run — settings, usage log, quota counters, deployment
> history and your anonymous device id all come across in one move, and the old
> directory is gone afterwards so nothing ends up split across two places. The
> welcome screen tells you it happened.
>
> **`TUISAMPLE_*` environment variables still work but are deprecated** — switch
> to `BOXCODE_*`. If you are still relying on an old name, the welcome screen
> names it. When both are set, `BOXCODE_*` wins.

```
 ◈

  ▟█▙       ▟█▙    boxcode  v1.5.0
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
curl -fsSL https://boxcode.sh/install.sh | bash
```

Windows (PowerShell):
```powershell
irm https://boxcode.sh/install.ps1 | iex
```

`boxcode.sh` serves the very same `install.sh`/`install.ps1` that sit in this
repo — it is published from `main` by `.github/workflows/pages.yml`, not
maintained as a second copy. If you'd rather fetch from GitHub directly,
`https://raw.githubusercontent.com/HolboxAI/boxcode/main/install.sh` still
works and always will.

Downloads a prebuilt binary for your platform (macOS/Linux/Windows,
x86_64/arm64) from the latest
[release](https://github.com/HolboxAI/boxcode/releases) and verifies it
against a published checksum — no Rust toolchain needed, installed in
seconds. On macOS/Linux it also installs Python's `ddgs` package if it's
missing, since `web_search` needs it.

> **Windows: `web_search` is temporarily off.** That step is disabled in
> `install.ps1` for now — on a machine with no Python it was failing the whole
> install rather than just costing one feature. Everything else works; if you
> want `web_search`, run `pip install ddgs` yourself. The code is commented
> out, not removed.

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
git clone https://github.com/HolboxAI/boxcode
cd boxcode
cargo build --release
./target/release/boxcode
```

### 2. Configure

Fastest way: launch `boxcode` and type `/provider` — pick a provider from the
list (arrow keys, Enter), then pick a model. If you already have that provider's
conventional API key exported (e.g. `DEEPSEEK_API_KEY` for DeepSeek,
`OPENAI_API_KEY` for OpenAI — pattern is `{PROVIDER}_API_KEY`), it's picked up
automatically; otherwise you're prompted to paste or type it (input hidden). The
choice is written to `~/.boxcode/config.toml` so it's remembered next launch.
Not on the list, or pointing at a self-hosted/internal endpoint? Pick
**"Custom endpoint..."** at the bottom of the list instead — you'll be walked
through endpoint, model, and API key manually, same as filling in the file by hand.

Alternatively, skip the picker and set environment variables or write
`~/.boxcode/config.toml` directly:

```toml
[llm]
endpoint = "https://llm.company.internal:8443"
model = "company-llm-70b-v1.2"
api_key = "sk_company_xxx"
```

Or use environment variables:
```bash
export BOXCODE_ENDPOINT=https://llm.company.internal:8443
export BOXCODE_MODEL=company-llm-70b-v1.2
export BOXCODE_API_KEY=sk_company_xxx
```

### 3. Run

```bash
boxcode
```

### 4. Update

**It offers, you decide.** Starting `boxcode` notices when a newer release
exists and asks:

```
⬆️  boxcode 1.2.0 is available (you have 1.1.2).
   Install it now? [y/N]
```

`y` runs the installer and tells you to start again; anything else carries
straight on. The default is **no** — an update prompt is not what you opened
the terminal for, so a stray Enter gets you to work rather than replacing the
binary underneath you.

The check is deliberately unobtrusive: at most **once a day**, a **2-second**
timeout, and completely silent when it fails or the network is unreachable.
It never appears when stdin is not a terminal, so scripts, CI and editor
integrations are unaffected. Turn it off with either:

```toml
[update]
check_on_start = false
```
```bash
export BOXCODE_NO_UPDATE_CHECK=1
```

To reinstall by hand at any time:

```bash
boxcode --upgrade
```

This is now always a **force** install — it reinstalls whether or not the
version number moved, which is what you want when `main` has changed without a
release or the install itself looks wrong. It removes stale copies from other
directories on your `$PATH` and confirms the shell resolves to the new build.
`--force` is still accepted and now does nothing extra.

> Upgrading from 0.2.0 or earlier? Those builds predate this flag — run the
> install command from step 1 once more, and `--upgrade` works from then on.

Running somewhere with no route to github.com? Point upgrades at a fork or an
internal mirror serving the same `Cargo.toml` and `install.sh`/`install.ps1`
(whichever your platform uses):

```bash
export BOXCODE_UPGRADE_URL_BASE=https://git.company.internal/boxcode/raw/main
```

## Running commands, and reading/writing files

The model has tools in the directory you launched from, so you can ask
about the actual project instead of pasting code in, and have it create or
change files directly instead of hand-encoding writes into shell commands:

```
> create hello.py and run it
📝 /Users/you/project/hello.py
Created it — printing "Hello, World!" and running it now.
$ python3 hello.py — 1 line
```

`read_file`/`write_file` handle reading and creating/overwriting a single
file. `grep_search` finds lines by content (a regular expression, searched
recursively) the way `glob` finds files by name — both read-only, so neither
stops to ask. `deploy_project` ships the project to Vercel or Netlify (see
[Deploying](#deploying)). `run_command` is for everything else — builds, tests,
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

### Plan mode: decide the approach before anything exists

Approving actions one at a time tells you *what* is about to happen, never
*why*. The decision that actually mattered — rewrite the router, or add one
endpoint — was made silently several prompts ago. `/plan` moves that decision
to the front:

```
❯ /plan
  Plan mode on. Nothing can be written, edited, or run unless it is
  read-only — ask for what you want and you'll get a plan to approve first.

❯ add rate limiting to the API
```

The model reads, lists, globs and runs read-only commands — none of which need
approval, because none of them can do anything — and then stops with a plan:

```
╭ Start on this plan? ─────────────────────────────────────────╮
│  Token bucket, in-process, keyed by API key.                 │
│                                                              │
│  - src/middleware/rate_limit.rs — new. Bucket struct,        │
│    refill on read, 429 when empty.                           │
│  - src/api/routes.rs — wrap the router in the new layer.     │
│  - src/config.rs — two fields: requests_per_minute, burst.   │
│                                                              │
│  ❯ y start   ·   n revise                                    │
╰──────────────────────────────────────────────────────────────╯
```

**`n`** sends it back and leaves you in plan mode: say what was wrong and it
proposes again. Nothing has touched your disk — a declined plan is never
written.

**`y`** saves the plan as a markdown file in your project, ends plan mode, and
starts the work. Every write and command still asks individually, as always;
approving a plan approves the *approach*, not a blank cheque.

### The plan is a file you own

An approved plan lands in `plan.md` at the top of your project as plain
markdown, and the model ticks each step off in it as the work gets done:

```markdown
---
title: Rate limiting for the items API
status: in-progress
created: 2026-08-11
base_commit: 3c21dfb
---

## Steps

- [x] 1. Add the limiter in src/rate_limit.py
- [ ] 2. Wrap the router in src/app.py
- [ ] 3. Add requests_per_minute + burst to src/config.py
```

Commit it if you want the approach reviewed before the code exists. Edit it by
hand whenever you like. And next time you open boxcode in that directory, the
plan is simply picked up — no command, nothing to select, because the file being
there *is* the state:

```
  model     deepseek-v4-flash
  cwd       ~/code/itemstore
  plan      2/4 — Rate limiting for the items API
  next      3. Add requests_per_minute + burst to src/config.py
```

Finishing with a plan is deleting the file. There is one per project, so
approving a different plan replaces it — and the approval box says which plan it
displaces, and how far that one got, before you can press `y`.

The plan records the commit it was written against, so carrying one on after the
project has moved warns you rather than confidently building against a codebase
that no longer matches.

While plan mode is on, `write_file` and `edit_file` are not in the tool list
sent to the model at all, and `run_command` is narrowed to commands that cannot
change anything. No setting reaches this — not `require_approval = false`. A
`PLAN` tag sits in the footer for as long as it's on.

`/plan` again turns it off, or start a session in it with `boxcode --plan`.
Full details, including why `cargo build` is refused while planning:
**[docs/plan-mode.md](docs/plan-mode.md)**.

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
max_steps = 40            # command rounds per prompt before the model must answer
```

Per-run: `BOXCODE_WORKSPACE=/path/to/project`, `BOXCODE_TOOLS_ENABLED=0`.

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

## Project memory

If a `BOXCODE.md` exists at the project root, its contents ride along in the
system prompt of **every request** -- standing notes about the project (build
commands, layout, conventions) that the model follows without re-deriving
them each session. `AGENTS.md` is honoured the same way, so a project already
carrying one for other tools works here unchanged.

Type `/init` and the model explores the project and writes a starter
`BOXCODE.md` -- through the ordinary write approval, so you read it before it
exists. It is a plain file you own: edit it by hand any time (changes are
picked up on the very next request, no restart), commit it so the whole team's
sessions start briefed, delete it to turn the feature off.

Keep it short. It is resent with every request, so every line has a running
cost; a file over ~16k characters is clipped rather than sent whole.

## Sessions survive the terminal

Everything said in a conversation is appended, as it happens, to a plain
JSONL file under `~/.boxcode/sessions/` — one file per conversation, local
only, never transmitted. Close the terminal mid-task (or lose it to a crash)
and nothing said before that moment is gone:

```bash
boxcode --resume     # pick up this directory's last session where it left off
```

`/resume` does the same mid-session, into a fresh conversation (`/new` first
if one is already going — silently discarding it is not this command's call).
Sessions are per-directory: a session recorded in one project is never
offered in another.

`/new` and `/compact` start a fresh session file rather than rewriting the
old one, so the conversation you abandoned or summarised is still there to go
back to. A launch that never says anything leaves no file behind.

## Deploying

You deploy by **asking**, not by typing a command:

```
❯ deploy this project to vercel
```

There is deliberately no `/deploy`. A deployment needs a provider and a target
to mean anything, and a sentence carries both — where a bare command would have
to ask for them in screens of its own before anything could start.

`deploy_project` is a tool like `run_command` or `write_file`, so this works in
the middle of a conversation. One approval prompt, then the URL — or the build
log — comes back to the model, so it can read the error, fix it, and try again.

### What it looks like

```
❯ deploy this project to vercel

  I'll deploy it to Vercel as a preview.

╭ Deploy this project? ────────────────────────────────────────────────╮
│ ⚠  PUBLISHES                                                         │
│ uploads this project to a third-party host and puts it on the public │
│ internet                                                             │
│                                                                      │
│ 🚀 vercel · Preview                                                  │
│ Vite · npm run build · dist                                          │
│                                                                      │
│ in /tmp/deploy-demo                                                  │
│                                                                      │
│ ❯ y deploy                                                           │
│   n skip                                                             │
╰──────────────────────────────────────────────────────────────────────╯

  (the deployment panel takes over here: install prompts, sign-in and the
   streaming build all happen in it)

  · 🚀 deploy → vercel (Preview) — https://my-app-x1.vercel.app

  Deployed. It's live at https://my-app-x1.vercel.app
```

A deployment **always stops for an explicit decision**, even with
`require_approval = false` — it is classified `Dangerous`, the same tier as
`rm -rf build`, because it sends your project to a third party and puts it on
the public internet. Previews are the default; the model has to be told
"production" to get it.

Once you press `y`, the deployment runs **interactively** rather than in a
headless executor. That matters, because it is what lets the two things a
deployment may need mid-run actually happen:

- **Installing a missing CLI.** The install prompt appears with the exact
  command and the guardrails' verdict on it, and waits for you.
- **Signing in.** The terminal is handed to the provider's own browser login
  and handed back afterwards.

Neither could happen inside a tool executor: one needs a prompt, the other
needs the terminal itself. So the model just calls the tool and the flow asks
you for whatever it needs, as it needs it. When it finishes, the URL -- or the
build log, if it failed -- goes back to the model, which then answers you.

Two limits worth knowing:

- **It must be the only tool call in the turn.** A deployment owns the screen
  until it finishes, so it cannot be interleaved with other calls. Asked for
  alongside others, it is declined with an explanation the model can act on.
- **It cannot set environment variables.** The schema accepts only `provider`
  and `production`, so a model has no way to name a secret, let alone invent a
  value for one. Those are typed by hand into the panel's masked field.

### The panel

Once approved, a panel takes over the bottom strip of the terminal and drives
the rest: the checklist of what has happened, any question that comes up, and
the build streaming live. The transcript stays visible above it the whole time,
and the UI never blocks.

The panel is a fixed height — the same strip the prompt normally occupies — so
the log scrolls inside it while the **status line and the keys stay pinned to
the bottom**. That is deliberate: the spinner and "esc to stop" are exactly
what you need while a build runs, and they are the first thing a panel that
simply grew would push off the screen.

```
╭ Continue with deployment? ───────────────────────────────────╮
│  ✔ Project validated (Vite)                                  │
│                                                              │
│  Project          my-app                                     │
│  Framework        Vite                                       │
│  Directory        /Users/you/my-app                          │
│  Provider         Vercel                                     │
│                                                              │
│  ❯ Yes                                                       │
│    No                                                        │
│                                                              │
│    ↑↓ choose · enter confirm · esc back                      │
╰──────────────────────────────────────────────────────────────╯
```

...and at the end:

```
╭ Deployment successful ───────────────────────────────────────╮
│  ✔ Project validated (Vite)                                  │
│  ✔ Vercel CLI 33.5.1                                         │
│  ✔ Signed in as ada                                          │
│  ✔ Project created                                           │
│  ✔ Built and uploaded                                        │
│  ✔ Confirmed live                                            │
│                                                              │
│  Deployment successful!                                      │
│                                                              │
│  🌐 Production URL                                           │
│  https://my-app.vercel.app                                   │
╰──────────────────────────────────────────────────────────────╯
```

**↑/↓** move · **Enter** confirms · **y**/**n** answer a two-choice screen
directly · **Esc** goes back a screen, and stops a deployment that is running.

### What it detects

Read from the filesystem only — no network, no package manager, nothing run:

| Detected | From | Build command | Output |
| --- | --- | --- | --- |
| Next.js | `next.config.*`, `next` | `npm run build` | provider-managed |
| Nuxt / Remix | their config or dep | `npm run build` | provider-managed |
| Astro | `astro.config.*`, `astro` | `npm run build` | `dist` |
| SvelteKit | `svelte.config.*` + `@sveltejs/kit` | `npm run build` | `dist` |
| Vite | `vite.config.*`, `vite` | `npm run build` | `dist` |
| React (CRA) | `react-scripts`, `react` | `npm run build` | `build` |
| Node.js | `main`/`start`, no build | none | none |
| Static HTML | `index.html` | none | `.` |

Rules that matter in practice: a config file on disk outranks a dependency,
and specific outranks general — a Next.js project also has React, and a
SvelteKit project also has Vite. The build command comes from your own
`package.json` `build` script where there is one, spelled for whichever package
manager your lockfile implies (`pnpm build`, `yarn build`, `bun run build`).
Output is left unset for frameworks both providers infer themselves; passing a
directory there would override a correct answer with a guess.

Everything is a default, not a decision — **"Edit configuration"** changes the
name, build command or output directory before anything runs.

### Authentication

Nothing here asks you to copy a secret into this app if it can avoid it. In order:

1. **`VERCEL_TOKEN` / `NETLIFY_AUTH_TOKEN` in your environment.** Used
   automatically, passed to the CLI through the child process's *environment*
   and never its argv (argv is world-readable via `ps`). Nothing to type.
2. **An existing CLI session.** If you have ever run `vercel login` or
   `netlify login` on this machine, `vercel whoami` / `netlify status` finds it
   and you are never asked anything.
3. **A browser login.** The app tears its own UI down, hands the real terminal
   to `vercel login` / `netlify login` so their own flow runs exactly as it
   normally would, and rebuilds the UI when it returns. No secret passes
   through this app at all.

Pasting a token into a masked field is offered as a last resort. It is kept in
memory for that one deployment and written nowhere.

A **"Sign out, then sign in"** option exists for the case a browser login alone
does not fix: a stale session from a rotated token or an account switched
elsewhere.

### The CLI

Deploying needs `vercel` or `netlify` on your `PATH`. If one is missing, it
offers to install it and shows the exact command and the guardrails' verdict on
it — `npm install -g` writes outside the project, so it is flagged as
destructive by the same `danger.rs` classifier every shell command goes through:

```
╭ CLI required ────────────────────────────────────────────────╮
│  The provider's CLI is not installed. Nothing is installed    │
│  without your say-so.                                         │
│  ⚠  DESTRUCTIVE                                               │
│  installs globally, outside the project                       │
│                                                               │
│  ❯ Yes                                                        │
│      npm install -g vercel                                    │
│    No                                                         │
╰───────────────────────────────────────────────────────────────╯
```

Nothing is ever installed without that confirmation. Set
`allow_cli_install = false` under `[deploy]` to remove the offer entirely — the
flow then tells you what to run instead of asking.

### Environment variables

Added by name, then value, with the value hidden as you type. They are passed
to the build through the child process's environment, never its argv, and:

- they are **never** printed, logged, echoed back, or written to the history;
- the variables screen shows `API_KEY = ••••••••` — a fixed-width mask, so even
  the length does not leak;
- streamed CLI output is scrubbed on the way in, so a token *the CLI* prints
  that this app never held is masked too.

> **Netlify builds locally** (`netlify deploy --build`), so these genuinely
> reach the build. **Vercel builds remotely** by default — it uploads the
> source and builds on its own infrastructure, so a variable set here reaches a
> local build step but not a remote one. For values Vercel needs at build or
> run time, set them on the project in the Vercel dashboard (or with
> `vercel env add`). This app deliberately does not shell out to `vercel env
> add`, because that CLI takes the value on the command line, where `ps` can
> read it.

### When it fails

The failure screen names the cause and offers a way forward rather than
dead-ending:

```
╭ Deployment failed ───────────────────────────────────────────╮
│  ✖  FAILED                                                    │
│  The build command failed on Vercel. The last lines of the     │
│  build log are above; run the same build locally to            │
│  reproduce it.                                                 │
│                                                               │
│  ❯ View detailed logs                                         │
│    Retry deployment                                           │
│    Cancel                                                     │
╰───────────────────────────────────────────────────────────────╯
```

A retry keeps everything already configured, including a token you supplied —
the usual reason to retry is a build error you just fixed in another window.

Recognised and explained separately: CLI missing, CLI present but broken,
signed out, credentials rejected, a name already taken, a site that no longer
exists, a build failure, a missing output directory, rate limiting, a timeout,
and a network failure. Anything unrecognised quotes the CLI's own last line
rather than inventing a summary — that is what you would actually search for.

### Deployment history

Every finished attempt is appended to `~/.boxcode/deployments.jsonl` — a plain
file you can read yourself:

```bash
cat ~/.boxcode/deployments.jsonl
```

It stores the project, path, provider, target, status, URL, timestamp and the
**names** of any environment variables. It stores no tokens and no values —
enforced by the shape of the record rather than by stripping: there is no field
a secret could go in, and `Secret` is not serialisable, so adding one later
fails to compile.

### Configuration

```toml
[deploy]
enabled = true            # false removes the deploy_project tool entirely
allow_cli_install = true  # false = never offer to install a provider CLI
history_limit = 10        # how many past deployments /deployments prints
```

### Limits worth knowing

- **This is not a sandbox either.** A provider CLI runs with your permissions
  and does what it does. The same honest caveat as the shell tool applies.
- **Cancellation kills the local process, not the remote build.** Esc stops
  `vercel deploy` on your machine; a build already accepted by Vercel keeps
  going on their side. Cancel it in their dashboard if that matters.
- **One project per run** — whatever directory the app was launched in.
  `BOXCODE_WORKSPACE=/path/to/project` points it elsewhere.
- **Windows is best-effort.** Commands route through `cmd /C` there, because
  `vercel`/`netlify`/`npm` are `.cmd` shims that `CreateProcess` will not run
  directly. That path has not been exercised on a real Windows machine.
- **Provider CLIs change.** Output parsing is defensive (JSON where it is
  offered, tolerant text reading otherwise, "unknown" rather than a guess when
  an answer is not recognised) but a sufficiently large CLI redesign will need
  the parsers updated.

### Adding another provider

One file implementing `DeploymentProvider`, plus one line in
`deploy::providers()`. Nothing in `app.rs` or `ui.rs` names a provider, so the
menus, progress panel and history pick it up with no UI change. See
`src/deploy/mod.rs` for why the trait describes commands rather than running
them, and `src/deploy/vercel.rs` for the shortest complete example.

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

Two things make a wrong guess survivable. Body text never picks a colour at
all — it takes the terminal's own foreground, which cannot clash with the
terminal's own background, so what you type is always readable. And every
other colour that carries words is contrast-checked in CI against *both*
backgrounds, so landing on the wrong palette costs vividness rather than
legibility.

## Usage

- **Type prompt** — Bottom input line (paste works too)
- **Enter** — Send prompt
- **Alt-Enter** / **Shift-Enter** — Insert a newline for multi-line prompts
- **Esc** — Cancel ongoing request
- **↑ / ↓** — Recall previous prompts. Inside a multi-line prompt they move
  between its lines first, so a stray ↑ can't swallow what you were writing
- **Your terminal's own scrollback** — the session is printed as ordinary
  output, so the wheel, text selection and your terminal's search all work on
  it, and it is still there after you quit
- **Ctrl-A / Ctrl-E** — Jump to start / end of line
- **Ctrl-W** — Delete previous word
- **Ctrl-U / Ctrl-K** — Delete to start / end of line
- **Ctrl-C** — Exit

`endpoint` may be given as `https://host`, `https://host/v1`, or the full
`https://host/v1/chat/completions` — all three resolve correctly. Environment
variables override values in `config.toml`.

### Slash Commands

- **`/plan`** — Toggles plan mode: the model researches and proposes a plan,
  and cannot change anything until you approve one. See "Plan mode" above.
- **`/provider`** — Opens a picker (↑/↓ to navigate, Enter to select, Esc to
  cancel) of built-in providers, plus a **"Custom endpoint..."** entry that
  preserves the "any OpenAI-compatible endpoint" support above — it's not
  limited to the built-in list. Selecting a provider chains straight into a
  model picker for it.
- **`/model`** — Re-picks just the model for whichever provider is currently
  configured, without going through `/provider` again. If no provider has been
  set yet (e.g. you're only using `BOXCODE_*` env vars or a custom endpoint),
  this shows an inline error telling you to run `/provider` first.
- **`/init`** — Has the model explore the project and write (or update) the
  `BOXCODE.md` that every later session reads -- see "Project memory" above.
  The write waits for your approval like any other.
- **`/resume`** — Reloads this directory's most recent recorded session and
  carries on from it — see "Sessions survive the terminal" above. Only into a
  fresh conversation; `boxcode --resume` does it from launch.
- **`/new`** — Forgets the current conversation. The configured provider and
  model are untouched; only the message history and tool-step count reset.
  The forgotten session's file stays on disk, so `/resume` can bring it back.
- **`/compact`** — Has the model summarise the conversation so far, then
  continues from that summary instead of the full transcript. Same problem
  `/new` solves — the whole history is resent every turn, so a long session
  costs more with each prompt — without the amnesia: what was established
  survives, at the price of one summarising request. It prints what that
  bought:

  ```
  Compacted the conversation.

    before    ~18,432 tokens  ·  42 messages
    after      ~1,240 tokens  ·  1 message
    freed     ~17,192 tokens  ·  93% smaller

  Today so far: 124,300 tokens over 37 requests, this summary included.
  Context figures are estimates at 4 characters per token, not billed counts.
  ```

  The summary is shown in full, since it is what the model will be working
  from next. Nothing is discarded until a usable summary comes back: an empty
  reply, a failed request, or Esc all leave the conversation exactly as it was.

  **It also happens by itself.** When the context passes a threshold — exact
  prompt tokens where the endpoint reports them, the 4-chars/token estimate
  otherwise — the next finished turn rolls straight into a compaction, with a
  notice first so you watch it happen. The same guarantees hold: nothing is
  discarded until a usable summary comes back, and a failed request never
  triggers one. Tune or disable it:

  ```toml
  [compact]
  auto = true             # false turns automatic compaction off
  auto_at_tokens = 80000  # context size that triggers it
  ```
  The request is metered like any other, and an exhausted `/quota` refuses it —
  otherwise the cheapest way past a spent allowance would be to know this
  command.
- **`/usage`** — Prints your token usage from `~/.boxcode/usage.jsonl`:
  today, the last 7 days, and all time. This is local and per-install only —
  there is no login, so it is the only place this number exists; nothing here
  is ever sent anywhere (see "Anonymous usage pings" below for the one thing
  that is).

`/provider` and `/model` write the result to `~/.boxcode/config.toml`
and apply it immediately — no restart needed, even mid-session.

### Anonymous usage pings

There is no login, so there is no way to attribute usage to a person — what
this app can see instead is a random ID generated once per install
(`~/.boxcode/device_id`), which labels a machine, not a person. Two
things, and only these two things, ever leave your machine:

- `install.sh`/`install.ps1` sends one `install` ping on a fresh install or an
  `--upgrade`.
- The app itself sends one `active` ping per calendar day (UTC) it's actually
  used, checked against `~/.boxcode/last_active` so a long session
  doesn't send more than one.

Each ping carries only `{anon_id, event, version, os, date}` — no prompts, no
file paths, no command text, no conversation content. Both are silent,
best-effort, and never block startup or fail an install: see `src/telemetry.rs`
and the `ping_install`/`Send-InstallPing` functions in `install.sh`/`install.ps1`.
`BOXCODE_TELEMETRY_URL=""` disables it on macOS/Linux; PowerShell cannot
represent an explicitly-blank environment variable (`$env:X = ''` deletes it
outright), so use `BOXCODE_TELEMETRY_URL=off` on Windows instead.

**The aggregate counts are public**: [tui-telemetry.dhruvm307.workers.dev](https://tui-telemetry.dhruvm307.workers.dev)
shows total installs, distinct anonymous devices seen, and daily-active counts,
live. That page is also the entire ingestion endpoint (see
`telemetry-worker.js` in the repo root) — it's as publicly *writable* as it is
readable, so treat the numbers as self-reported, not verified.

Set `BOXCODE_TELEMETRY_URL=""` (explicitly blank, not just unset) before
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
- `src/session.rs` — Conversation persistence (`~/.boxcode/sessions/`, `/resume`), local only
- `src/usage.rs` — Local per-install token usage log (`/usage`), never transmitted
- `src/telemetry.rs` — Anonymous install/daily-active pings, disabled by default
- `src/dateutil.rs` — Calendar-date helpers shared by the two above
- `src/deploy/` — shipping the project to a hosting provider
  - `mod.rs` — The `DeploymentProvider` trait, shared types, and `Secret`
  - `detect.rs` — Framework/build/output detection, from the filesystem alone
  - `cli.rs` — Is the provider's CLI installed, and may we install it
  - `runner.rs` — The one place a deployment subprocess is spawned: streaming,
    timeouts, cancellation, redaction
  - `service.rs` — The whole flow as a state machine, with no I/O in it
  - `history.rs` — `~/.boxcode/deployments.jsonl` (`/deployments`)
  - `vercel.rs` / `netlify.rs` — One file per provider
- `telemetry-worker.js` — The Cloudflare Worker that `telemetry.rs`/`install.sh` ping and that serves the public view
- `src/plan.rs` — The project's `plan.md`: the markdown format, progress tracking, and reading it back
- `docs/plan-mode.md` — Plan mode: what it guarantees, how to use it, and what it deliberately does not claim
- `docs/index.html` — The [boxcode.sh](https://boxcode.sh) landing page, deployed with the installers by `.github/workflows/pages.yml`

## What's Next

- A diff preview when a command is about to modify tracked files
- Remembering per-command approvals across a session
- GitHub integration (VPC-only)
- Test generation
- More deployment providers behind the existing `DeploymentProvider` trait —
  AWS Amplify, Cloudflare Pages, GitHub Pages, Render

## Development

```bash
# Build
cargo build --release

# Run with debug logging
RUST_LOG=debug boxcode

# Test (if you add tests)
cargo test
```

## License

MIT
