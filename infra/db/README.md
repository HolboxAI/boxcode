# boxcode db control-plane

Gives a published boxcode artifact basic persistence — a form that saves
somewhere, a table of products, anything beyond a static page — without
leaving boxcode to stand up Postgres/Supabase/Firebase. One SQLite file
per project, no ORM, no migration framework: the model sends one SQL
statement at a time and this relay runs it against that project's file.

Deliberately not Postgres: no separate database process to network into,
no migration system, no schema-namespace conventions to get right — all
real, live-debugged costs paid on the auth side (see `infra/auth/`'s own
history) for a third-party server (GoTrue) with its own opinions about all
three. Nothing here is a third-party binary; it's ~150 lines this repo
owns outright, using `node:sqlite` (built into the Node runtime already on
this box — no new dependency).

## Layout

- `control-plane/index.mjs` — the one always-running process. `POST
  /query {project_id, key, sql, params}` opens/creates that project's
  SQLite file and runs the one statement. No per-project process or port
  (unlike GoTrue): isolation comes from opening a different file per
  request, not a different server instance.
- `control-plane/boxcode-db-control-plane.service` — the systemd unit, its
  own port (8081).
- `setup.sh` — adds this to a box that has already run
  `infra/auth/setup.sh`. Writes its nginx route into
  `/etc/nginx/conf.d/auth-projects/_db-route.conf` — the directory the
  auth vhost already `include`s — rather than touching
  `/etc/nginx/conf.d/auth.conf` itself, which certbot edits in place once
  TLS is set up; overwriting that file from a template would silently
  destroy certbot's edit.

## The key: trust-on-first-use, and why it's never in the model's hands

Every request needs a per-project key. It's generated and persisted by the
**boxcode client**, not here — `~/.boxcode/db.json` on the developer's own
machine, keyed by project id, same pattern as `artifacts.rs`'s
`remembered_id`. The model that writes the published page's JavaScript
never sees this key, specifically so it can never end up embedded in
client-side code by accident — the failure mode an "open by project id
alone" design would have (any visitor to the live site gets arbitrary SQL
execution via browser devtools, not just injection into one query).

This relay never generates a key itself. The first `/query` call for a
project id it hasn't seen adopts whatever key that call supplies as the
project's key from then on; every later call must match it exactly or
gets a 403. **Known limitation**: this means the first client to query a
given project id wins — a genuine race if two different people queried
the same brand-new project id at the same moment, which the
single-developer-per-project "prove it works" phase this was built for
doesn't hit in practice. Worth revisiting if that assumption stops
holding.

## Statement shape

One statement per call — `.prepare()` only ever runs one, unlike `exec()`
which accepts several semicolon-separated but returns nothing useful from
any of them (confirmed locally before this was ever deployed). A
statement starting with `SELECT`/`PRAGMA`/`EXPLAIN` runs as a read and
returns `{rows, truncated}` (capped at 500 rows — `web_search`'s row cap
is the same idea); everything else runs as a write and returns
`{changes, last_insert_rowid}`.

## Known limitations

- No query timeout. `node:sqlite`'s `DatabaseSync` is synchronous —
  blocking the whole process for the duration of a query — so a
  JS-level timeout can't actually interrupt one already running. Not
  expected to matter for the workloads this is built for (simple
  CRUD against small per-project files), but worth knowing rather than
  assuming away.
- No security hardening beyond the key check: the control-plane runs as
  root, and a valid key grants unrestricted SQL (including `DROP TABLE`)
  against that one project's file. Same "prove it works first" tradeoff
  auth's README documents.
- SQLite's own concurrency limits (single-writer file locking) apply.
  Fine for the traffic this was built to prove out; a real concern if a
  project's data workload ever gets genuinely concurrent.
