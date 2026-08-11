# Testing Guide

## What Gets Tested

### 1. Unit Tests (`cargo test`)

Roughly 500 tests, almost all of them living beside the code they cover in a
`#[cfg(test)] mod tests`. The broad areas:

- Config loading, TOML parsing, env-var precedence, and upgrade safety (an
  older `config.toml` missing a whole table must still load)
- SSE streaming and tool-call reassembly, against a real socket fed a few bytes
  at a time so lines and multi-byte characters split across chunks
- The `danger.rs` command classifier — blocked, destructive, and ordinary
- Tool execution: `read_file`, `write_file`, `edit_file`, `list_dir`, `glob`,
  `web_search`, including the workspace-escape refusals
- Quota arithmetic, usage history, and the readouts
- Terminal rendering, swept across sizes from 1×1 to 200×60 so no screen can
  panic on a terminal too small to hold it, plus WCAG contrast checks on every
  palette against both backgrounds
- Deployment (`src/deploy/`) — see below

**Run:**
```bash
cargo test --all
```

### 1a. Deployment tests

The deployment flow is deliberately testable with **no CLI installed, nobody
signed in and no network**: providers describe commands rather than running
them, and `service.rs` is a pure state machine fed canned `CommandOutput`s.

```bash
cargo test --bin boxcode deploy      # everything under src/deploy/
cargo test --bin boxcode detect      # framework detection only
```

What is covered:

| Area | Where |
| --- | --- |
| The v1.0.0 state migration: nothing lost, nothing mixed | `paths::tests` |
| `BOXCODE_*` / deprecated `TUISAMPLE_*` precedence | `paths::tests` |
| Project/framework detection, ordering, build+output defaults | `deploy::detect` |
| CLI detection: present, missing, present-but-broken | `deploy::cli` |
| Install command judged by the `danger` guardrails | `deploy::cli` |
| Provider selection and the whole flow end to end | `deploy::service` |
| Authentication state: signed in, out, and unrecognised | `deploy::vercel`, `deploy::netlify` |
| Deployment configuration, editing, env vars | `deploy::service` |
| Successful deployment, URL extraction, history record | `deploy::service` |
| Every failure path, retry, and cancellation | `deploy::service` |
| Command running, streaming, timeout, kill | `deploy::runner` |
| Secrets never reaching argv, the UI, or the history | `deploy` (several) |
| Every deployment screen rendering at every size | `ui::tests` |
| Deployment is not reachable as a slash command | `app::tests` |
| The panel fits a short viewport with its status line pinned | `ui::tests` |
| The `deploy_project` tool: args, risk, refusals | `tools::tests` |
| A deployment always stops for approval, even unattended | `tools::tests` |
| An approved tool call hands off to the interactive flow | `app::tests` |
| The model gets the URL, the build log, or the cancellation back | `app::tests` |
| A deployment batched with other calls is declined, not sequenced | `app::tests` |

The runner's tests spawn real processes, but only `sh`/`cmd` — never `vercel`
or `netlify`. Nothing in `cargo test` reaches the network or deploys anything.

### 2. System Requirements Tests
- ✅ Cargo/Rust must be installed
- ✅ Config directory can be created (~/.boxcode)
- ✅ Env var format validation

**These will SKIP if requirements aren't met** (not failures)

### 3. Manual Testing (Before Release)

#### Prerequisites
Before testing, you need:
1. **Rust/Cargo installed** - https://rustup.rs/
2. **An LLM endpoint** - OpenAI, Bedrock, or self-hosted
3. **Valid API credentials**

#### Test Procedure
```bash
# Build from source
cargo build --release

# Set environment variables
export BOXCODE_ENDPOINT=https://api.openai.com
export BOXCODE_MODEL=gpt-4
export BOXCODE_API_KEY=sk-...

# Run the application
./target/release/boxcode
```

#### Expected Behavior
1. TUI launches (header, messages area, input box, status)
2. Can type prompt in input area
3. Press Enter to send
4. Response streams from LLM
5. Can cancel with Esc
6. Exit with Ctrl-C

### 4. Install Script Testing
```bash
# This requires Cargo to be installed
bash <(curl -fsSL https://boxcode.sh/install.sh)
```

**Error: "Rust/Cargo not found"?**
→ Install from https://rustup.rs/

### 3a. Manual Deployment Testing

The one thing unit tests genuinely cannot cover is a real deployment. To
exercise it end to end:

```bash
# A throwaway project to ship
mkdir -p /tmp/deploy-demo && cd /tmp/deploy-demo
npm create vite@latest . -- --template react
npm install

# Point the app at it and launch
BOXCODE_WORKSPACE=/tmp/deploy-demo boxcode
```

Then ask it: `deploy this to vercel`. Approve the prompt, and walk the panel:
confirm the detected Vite setup, skip the environment variables, choose Preview
(not Production, for a test), and let it run.

Worth checking by hand, since no test can:

1. **A browser login.** With no `VERCEL_TOKEN` and no prior `vercel login`,
   choosing "Log in with a browser" should tear the TUI down, hand over cleanly,
   and rebuild the screen intact afterwards.
2. **Cancellation.** Press Esc mid-build; the local CLI process should die
   (`ps aux | grep vercel` shows nothing).
3. **A failing build.** Break the build script (`"build": "exit 1"`) and confirm
   the failure screen names the cause and that Retry works after fixing it.
4. **Secrets.** Add an environment variable with a recognisable value, deploy,
   then check it appears nowhere:
   ```bash
   grep -r "your-test-value" ~/.boxcode/    # must find nothing
   ```

## CI/CD Tests
GitHub Actions runs:
- `cargo build --release` - Compilation
- `cargo test --all` - Unit tests
- `cargo clippy` - Linter suggestions

**Status:** View at https://github.com/HolboxAI/boxcode/actions

## What's NOT Tested Automatically
- A real deployment to Vercel or Netlify (needs an account and a live network)
- The browser-login handover, which by definition needs a human at a browser
- Anything on Windows — the `cmd /C` path in `deploy::runner` and the
  PowerShell installer are exercised by CI's build matrix but not by a person
- Streaming responses against a real LLM endpoint

The TUI itself *is* tested: `ui.rs` renders every screen into a `TestBackend`
across terminal sizes from 1×1 upward.

## Troubleshooting

| Error | Cause | Fix |
|-------|-------|-----|
| "Rust/Cargo not found" | Rust not installed | Install from https://rustup.rs/ |
| "Could not connect to endpoint" | LLM endpoint invalid | Check BOXCODE_ENDPOINT env var |
| "Authentication failed" | Invalid API key | Check BOXCODE_API_KEY env var |
| "Cannot find model" | Model name typo | Check BOXCODE_MODEL env var |
| "No project directory" when deploying | `[tools] enabled = false` | Enable tools, or set `BOXCODE_WORKSPACE` |
| "nothing to build or serve" | No `package.json` and no `index.html` | Launch boxcode from the project directory |
| CLI "installed but is not working" | Broken global npm install | `npm install -g vercel` again, or check `vercel --version` by hand |

## Adding New Tests
When adding features:
1. Add unit test in `tests/`
2. Run `cargo test --all` locally
3. Verify CI passes
4. Create PR with tests

**Do NOT commit without tests!**
