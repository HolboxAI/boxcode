# Testing Guide

## What Gets Tested

### 1. Unit Tests (`cargo test`)
- Config file parsing (TOML format), including configs with no `[agent]` table
- Environment variable handling
- Streaming: SSE parsing, and tool-call fragments reassembled across TCP chunks
- Tools: each one against a temp workspace — happy path, path-escape rejection,
  output truncation, `edit_file` refusing an ambiguous match
- Permissions: what runs unattended, and how session grants are scoped
- The agent loop end to end against a fake endpoint (see below)
- UI: entry rendering, and overlays in a terminal too small to hold them
- String buffer operations

**Run:**
```bash
cargo test --all
```

### 1a. The agent loop, without a live model

`src/agent/run.rs`'s tests serve canned SSE responses off a local socket, so the
whole loop — real HTTP, real streaming, real tool dispatch, real file writes — is
covered with no endpoint and no API key. That includes the cases that are painful
to reproduce by hand:

- a denied write: the file is not created, the model is told, the run continues
- "allow for session" suppressing the second prompt for the same tool
- a tool the agent isn't allowed refused before it reaches the permission gate
- truncated tool-call JSON reported as a readable error
- the iteration cap ending a model that never stops calling tools
- cancellation mid-run still answering every outstanding `tool_call`, so the
  conversation stays valid to continue from

Add to these rather than reaching for a live endpoint — they run in about a second.

### 2. System Requirements Tests
- ✅ Cargo/Rust must be installed
- ✅ Config directory can be created (~/.tuisample-code)
- ✅ Env var format validation

**These will SKIP if requirements aren't met** (not failures)

### 3. Manual Testing (Before Release)

#### Prerequisites
Before testing, you need:
1. **Rust/Cargo installed** - https://rustup.rs/
2. **An LLM endpoint** - OpenAI, Bedrock, or self-hosted
3. **Valid API credentials**

#### Test Procedure

Work in a throwaway repo — the agent writes to real files.

```bash
cargo build --release

export TUISAMPLE_ENDPOINT=https://api.deepseek.com
export TUISAMPLE_MODEL=deepseek-v4-pro
export TUISAMPLE_API_KEY=sk-...

mkdir /tmp/scratch && cd /tmp/scratch && git init && cargo init
/path/to/tuisample-code
```

Then ask: **"add a `greet` function to src/main.rs and a test for it"**

#### Expected Behavior
1. TUI launches (header with workspace name, messages area, input box, status)
2. Typing a prompt and pressing Enter starts a run; status shows `Working…`
3. `read_file` / `glob` / `grep` calls appear as `● name(args)` and run without asking
4. Before the first write, an **Approve action** popup appears; status shows
   `Needs approval`
5. `a` allows it once — the file is actually written
6. `s` at the `cargo test` prompt allows every `cargo` command; a later
   `cargo build` does not prompt again
7. `d` denies — the agent is told and adapts rather than crashing
8. Esc mid-run cancels; verify no orphaned child survives: `pgrep -f 'cargo test'`
9. `/new` forgets the conversation; the transcript stays on screen
10. Exit with Ctrl-C

#### Things worth trying to break
- Ask it to read `../../etc/passwd` — must be refused, not read
- Ask it to `rm -rf /` — must prompt, and must *not* offer "allow for session"
  for a compound command
- Resize the terminal very small while an approval popup is up — must not panic
- Point at an endpoint with no function-calling support — the model should still
  answer, just without touching files

### 4. Install Script Testing
```bash
# This requires Cargo to be installed
bash <(curl -fsSL https://raw.githubusercontent.com/HolboxAI/tuisample-code/main/install.sh)
```

**Error: "Rust/Cargo not found"?**
→ Install from https://rustup.rs/

## CI/CD Tests
GitHub Actions runs:
- `cargo build --release` - Compilation
- `cargo test --all` - Unit tests
- `cargo clippy` - Linter suggestions

**Status:** View at https://github.com/HolboxAI/tuisample-code/actions

## What's NOT Tested Yet
- Keyboard input through a real terminal (the `App` state machine is covered, but
  crossterm's event decoding is not)
- Behaviour against a real model — whether it actually reaches for the right tool
  is a prompt-quality question no unit test answers
- How tool output reads at various terminal widths

Streaming and TUI rendering *are* covered: streaming against a local socket, and
rendering via ratatui's `TestBackend` — including 0x0 and other sizes too small
to hold an overlay, which used to panic.

## Troubleshooting

| Error | Cause | Fix |
|-------|-------|-----|
| "Rust/Cargo not found" | Rust not installed | Install from https://rustup.rs/ |
| "Could not connect to endpoint" | LLM endpoint invalid | Check TUISAMPLE_ENDPOINT env var |
| "Authentication failed" | Invalid API key | Check TUISAMPLE_API_KEY env var |
| "Cannot find model" | Model name typo | Check TUISAMPLE_MODEL env var |
| Agent describes changes instead of making them | Endpoint ignores the `tools` field | Confirm the endpoint supports function calling |
| "Stopped after N tool rounds" | Task too broad, or the model is looping | Raise `[agent] max_iterations`, or narrow the task |
| "resolves outside the workspace" | Agent tried to leave the launch directory | Working as intended — relaunch from the right root |
| "truncated: hit max_tokens" | Turn cut off mid-answer | Raise `[agent] max_tokens` |

## Adding New Tests
When adding features:
1. Add unit test in `tests/`
2. Run `cargo test --all` locally
3. Verify CI passes
4. Create PR with tests

**Do NOT commit without tests!**
