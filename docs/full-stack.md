# Full-stack projects

`publish_artifact` puts a static frontend on a public URL. `deploy_backend` puts
a **real server** behind it, at the same domain, with a PostgreSQL database.

```
https://boxcode.sh/artifacts/k9depef6      the frontend, static
https://boxcode.sh/api/k9depef6/           the backend, a running server
```

Both expire **48 hours** after they are deployed, together.

## Read this before you write the code

These are properties of how the hosting works, not gaps waiting to be filled.
Finding out about them after building around the opposite assumption is the
expensive way.

### There is no outbound internet

The server **cannot make outbound network calls**. Not to Stripe, not to OpenAI,
not to SendGrid, not to an SMTP host, not to any third-party API, not to a
webhook. DNS for external names does not resolve.

This is the single most valuable control in the design and it is not
configurable: a hosted project cannot reach a mining pool, a command-and-control
server, a spam relay, or anywhere to send data it has collected. The cost is that
genuine integrations do not work either, and there is no way to grant an
exception for one.

If a project needs to call something external, that call has to happen somewhere
this platform is not.

### The database is PostgreSQL, and it is real

`DATABASE_URL` is in the server's environment and points at a database created
for this project alone. Prisma, SQLAlchemy, the Django ORM, TypeORM, Drizzle,
`pg`, `psycopg` — anything that speaks Postgres works normally. Run migrations,
define a schema, use transactions.

The role has no privileges outside its own database, is capped at 5 concurrent
connections, and has a 10-second statement timeout.

### Listen on `PORT`

The environment sets `PORT`. Bind to it, and to `0.0.0.0` — not `127.0.0.1`, and
not a hard-coded number.

```js
app.listen(process.env.PORT || 3000, "0.0.0.0");
```

Getting this wrong is the most common reason a deploy reports success and the URL
does not answer. The deploy checks the URL afterwards and tells you when that
happens, rather than claiming success.

### Dependencies are installed on the server

From `package.json` or `requirements.txt` / `pyproject.toml`. `node_modules` and
virtualenvs are **never uploaded** — they are rebuilt against the runtime the
server actually has, which is also why a native module built on macOS is not a
problem here.

A `package-lock.json` is used when present, so what deploys is what was tested.

`.env` and `.env.local` are **never uploaded**. Anything secret has to arrive
another way, and on a 48-hour demo host the honest answer is usually that it
should not be there at all.

### Ten at a time, across everyone

A refusal saying the limit is reached is a real answer, not a retryable error. It
says when the next slot frees.

One deploy token may hold **2 live projects**. The token is minted on this
machine on first deploy and never rotated — it is what proves a redeploy is
coming from whoever deployed the project originally.

## Calling the backend from the frontend

Use a **root-absolute** path:

```js
fetch(`/api/k9depef6/todos`)
```

Not `fetch("api/todos")` — the frontend is served from `/artifacts/<id>/`, so a
document-relative path resolves to `/artifacts/<id>/api/todos`, which is not the
backend.

Because both halves are on the same origin there is **no CORS to configure**, no
preflight, and cookies work normally.

> **No build-time API URL is injected**, and that is deliberate. A `VITE_API_URL`
> baked in at build time would have to be re-baked whenever the project id or the
> domain changed, and it only ever holds a string the page could have written
> literally. The root-absolute path needs no build step, no environment variable,
> and no rebuild if the domain moves.

## What gets detected

The framework, runtime and entrypoint are read from the directory:

| | |
|---|---|
| Node.js | Express, Fastify, Koa, NestJS |
| Python | FastAPI, Flask, Django |

Anything else that is recognisably a server still deploys — the start command is
taken from the entrypoint. Django is found by `manage.py`, but `manage.py` is
never the entrypoint: it is Django's CLI, not its app.

If the entrypoint cannot be worked out, the deploy stops and says so rather than
starting something that was never going to serve.

## Isolation

Each project runs in its **own Firecracker microVM**, with its own guest kernel —
the same hypervisor AWS built for Lambda and Fargate. Projects are separated by
hardware virtualisation, not by a shared kernel's permission checks, and they
cannot see or reach each other.

The build runs in a microVM too. `npm install` executes arbitrary `postinstall`
code, and containment is the only real answer to that.

## When something goes wrong

**The deploy says NOT confirmed.** The upload and start succeeded but the URL did
not answer. Almost always the `PORT` / `0.0.0.0` problem above.

**The deploy fails while installing.** The reason comes back from the build —
usually a package that needs a compiler, or a lockfile that disagrees with
`package.json`.

**The deploy is refused.** The message says which limit: the project cap, the
per-token cap, a rate limit, or the box being full. Each says what to do.

**It worked and then stopped.** Check the 48 hours. Deploy again to get another
48; the same project id keeps the same URL.
