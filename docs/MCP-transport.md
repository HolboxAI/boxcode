# MCP transport: what a server must do, and how to swap the fake for a real one

## Status

| Piece | State |
| --- | --- |
| stdio transport (local child process) | **Works** — connect, `initialize`, `tools/list` |
| streamable-http transport (remote URL) | **Not implemented** — returns `"streamable-http servers are not implemented yet"` |
| tools merged into the model's schema list | **Not done** |
| dispatching a `tools/call` | **Not done** — nothing invokes a tool yet |

Every hosted server (GitHub, Gmail, AWS) is a *remote* URL server, so none of
them can be reached until the http transport exists. A token alone changes
nothing.

## The contract the client relies on

`src/mcp.rs` is the only thing that talks to a server. A server must:

- accept **newline-delimited JSON-RPC 2.0** on stdin: one JSON object per line,
  terminated by `\n`. There is **no** `Content-Length` header framing.
- write one JSON object per line to stdout. A line that is not valid JSON is
  **skipped**, not fatal — but do not rely on that.
- keep stdout for protocol only and log to **stderr**, which is forwarded to
  `tracing` under `target: "mcp"`.
- answer `initialize`, then `tools/list`, and later `tools/call`.
- **echo the request's `id`** in the reply.

Tool records returned by `tools/list`:

```json
{ "name": "echo", "description": "...", "inputSchema": { "type": "object" } }
```

`description` is optional; `inputSchema` must be a JSON object.

## The fake server

`src/bin/mcp-fake-server.rs` implements that contract and nothing else. It is
reached only through a server config's `command`, so the client is unaware of
it. Environment knobs let one binary simulate the awkward cases:

| Variable | Effect |
| --- | --- |
| `MCP_FAKE_TOOLS` | comma-separated tool names to advertise (default `echo,add`) |
| `MCP_FAKE_NO_TOOLS` | advertise an empty list |
| `MCP_FAKE_FAIL_INIT` | answer `initialize` with a JSON-RPC error |
| `MCP_FAKE_SILENT` | never reply — exercises the request timeout |

## What the fake deliberately does not cover

- **http / remote servers** — the transport that the hosted servers need.
- authentication: `Authorization` headers, OAuth flows, token refresh.
- real network failure, TLS, redirects, rate limits.
- pagination of `tools/list` (`nextCursor` is not modelled).
- concurrency: the fake is strictly one request at a time.

Passing against the fake therefore proves the **framing and handshake**, not
that any particular real server works.

## Swapping in a real server

1. Pick a server and find its documented command/URL.
2. Write the config. User scope (`~/.boxcode/mcp.json`) is auto-approved;
   workspace scope (`<cwd>/.boxcode/mcp.json`) is withheld unless an `approve`
   policy is supplied.

   ```json
   {
     "mcpServers": {
       "github": {
         "type": "stdio",
         "command": "docker",
         "args": ["run", "-i", "--rm", "-e", "GITHUB_PERSONAL_ACCESS_TOKEN",
                  "ghcr.io/github/github-mcp-server"],
         "env": [{ "name": "GITHUB_PERSONAL_ACCESS_TOKEN",
                   "value": "${env:GITHUB_PERSONAL_ACCESS_TOKEN}" }]
       }
     }
   }
   ```

3. Export the variable the config references, then re-run.
4. For a **remote** server, the http transport must be implemented first —
   `headers` is currently parsed and validated for shape only, and never sent.

To point the test suite at a real server instead of the fake, change the
`command`/`args` (and `env`) built in the test that spawns it; that is the only
coupling.

## The environment-variable trap

An unset `${env:VAR}` is left **verbatim**, not blanked — deliberate, since
silently emptying it would turn a config mistake into a server that starts,
looks healthy and behaves differently. So an unset
`GITHUB_PERSONAL_ACCESS_TOKEN` sends the literal string
`${env:GITHUB_PERSONAL_ACCESS_TOKEN}` to the server, which fails *inside* the
server rather than at load time. A warning is logged; watch for it.

## Why this binary cannot ship

It lives in `src/bin/`, so `cargo build` compiles it, but it is not linked into
the `boxcode` binary and releases package the `boxcode` executable by name.
It is test-only: nothing in the shipped code path references it.
