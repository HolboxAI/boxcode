# Testing Guide

## What Gets Tested

### 1. Unit Tests (`cargo test`)
- Config file parsing (TOML format)
- Environment variable handling
- JSON response deserialization
- String buffer operations
- Daily usage quota: counting, persistence, midnight rollover, enforcement

**Run:**
```bash
cargo test --all
```

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
```bash
# Build from source
cargo build --release

# Set environment variables
export TUISAMPLE_ENDPOINT=https://api.openai.com
export TUISAMPLE_MODEL=gpt-4
export TUISAMPLE_API_KEY=sk-...

# Run the application
./target/release/tuisample-code
```

#### Expected Behavior
1. TUI launches (header, messages area, input box, status)
2. Can type prompt in input area
3. Press Enter to send
4. Response streams from LLM
5. Can cancel with Esc
6. Exit with Ctrl-C

#### Daily quota
```bash
# Set a limit of one request, then send two prompts.
TUISAMPLE_MAX_REQUESTS_PER_DAY=1 ./target/release/tuisample-code
```
1. After the first reply, the header shows `today: 1 req · … tok`
2. The second prompt is refused, naming the limit and the reset time
3. The refused prompt is **still in the input box** — nothing typed is lost
4. `/quota override` allows it through; `/usage` shows the override as active
5. `~/.tuisample-code/usage.json` holds the counts and survives a restart

Note: if the endpoint does not support `stream_options.include_usage`, token
counts render with a `~` prefix to mark them as local estimates. Requests are
always exact. Spend shows `$ unpriced` until the model is given a price under
`[quota.pricing]` in config.toml.

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
- Interactive terminal UI (hard to automate)
- Streaming responses (requires live LLM connection)
- Edge cases in TUI rendering

These require manual testing with a real LLM endpoint.

## Troubleshooting

| Error | Cause | Fix |
|-------|-------|-----|
| "Rust/Cargo not found" | Rust not installed | Install from https://rustup.rs/ |
| "Could not connect to endpoint" | LLM endpoint invalid | Check TUISAMPLE_ENDPOINT env var |
| "Authentication failed" | Invalid API key | Check TUISAMPLE_API_KEY env var |
| "Cannot find model" | Model name typo | Check TUISAMPLE_MODEL env var |

## Adding New Tests
When adding features:
1. Add unit test in `tests/`
2. Run `cargo test --all` locally
3. Verify CI passes
4. Create PR with tests

**Do NOT commit without tests!**
