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
owns outright, using `node:sqlite`.

`node:sqlite` needs Node 22.5+. The system `node` this box already has
(from `infra/auth/setup.sh`'s `dnf install nodejs`) turned out to be
v18.20.8 — confirmed live, `dnf module list nodejs` has no newer stream
to switch to on this box either — so `setup.sh` downloads a real Node
tarball from nodejs.org into `/opt/node24` and points only this
service's systemd unit at it, rather than upgrading the system node the
auth control-plane already depends on and works fine with. So: one real
new thing on the box (a second Node install), but still zero new npm
dependencies for this service's own code.

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

## Scoping a query to the signed-in user

The key above proves which *project* a request belongs to, not which of
that project's own users is asking — on its own, "only show my rows" has
to be SQL the model writes trusting a user id the page's own client-side
JS supplies as a param, which any visitor can forge from devtools. This is
the same gap Postgres RLS (`auth.uid()`) closes for Supabase; the version
here is much smaller, on purpose.

A request may include `access_token` — the same token `enable_auth`'s
`{auth_url}/token` endpoint hands back on sign-in (see `infra/auth/`).
When present, this relay spends one request against that project's own
GoTrue (`AUTH_BASE/${project_id}/user`) to turn the token into a verified
user id, and — only if `sql` actually references it — binds that id as the
named parameter `:current_user_id` (`@current_user_id`/`$current_user_id`
also work; `node:sqlite`'s own sigil rules). A bad or expired token is
rejected with 401 before the query ever runs, regardless of whether the
query needed it — sending an access_token is a signal the caller wanted it
checked, so a wrong one fails closed rather than silently running
unscoped. A query that references `:current_user_id` with **no**
access_token sent at all is not an error either: `node:sqlite` binds an
absent named parameter as `NULL`, which matches no real row, so it fails
closed too (returns nothing) rather than leaking every project's rows.

Not row-level security: this is one placeholder, not a policy engine —
there's nothing stopping a query from ignoring `:current_user_id` entirely
and reading every row anyway if the model's SQL doesn't filter on it. It
verifies *who's asking*, not what they're allowed to see; the model still
has to write the `WHERE` clause. Closing that fully would mean a real
policy layer, which is deliberately out of scope for the same "prove it
works" phase the rest of this README describes.

## `/db/named-query`: letting the page itself reach in, safely

`/query` needs the project's key, which is why the model that writes a
published page's client-side JS can never be handed it — that key
authorizes *arbitrary* SQL, and a key that could do that cannot safely
reach a browser. But that also means, until this route existed, a signed-in
visitor's own page had no way to read or write their own data at all: the
key gap that protects the database also blocked the one thing a real
"account" is supposed to do.

`POST /db/named-query {project_id, access_token, name, params}` closes
that gap without reopening it: no project key, ever — verified purely by
the caller's own `access_token`, same `verifyUser` check `/query`'s own
`access_token` support already does. What makes this safe to leave
key-less is that it never accepts SQL from the caller, only a `name`. The
only statements it will ever run are ones the developer already wrote and
registered themselves, through the key-authorized `/query` route, into an
ordinary table in their own project's database:

```sql
CREATE TABLE IF NOT EXISTS __boxcode_named_queries__ (
  name TEXT PRIMARY KEY,
  sql  TEXT NOT NULL
);
INSERT INTO __boxcode_named_queries__ VALUES
  ('my_todos', 'SELECT id, text FROM todos WHERE user_id = :current_user_id');
```

A visitor can only ever invoke a query their own developer chose to
expose by name; they can never write one from devtools. `:current_user_id`
binds exactly the same way `/query`'s does — verified against the
project's own GoTrue, not trusted from `params`. The same caveat as
`/query`'s own scoping applies here too: this is not row-level security,
just verification of *who's asking*; a registered query that doesn't
reference `:current_user_id` in its own `WHERE` clause will still return
every row to whoever calls it.

## Statement shape

One statement per call — `.prepare()` only ever runs one, unlike `exec()`
which accepts several semicolon-separated but returns nothing useful from
any of them (confirmed locally before this was ever deployed). A
statement starting with `SELECT`/`PRAGMA`/`EXPLAIN` runs as a read and
returns `{rows, truncated}` (capped at 500 rows — `web_search`'s row cap
is the same idea); everything else runs as a write and returns
`{changes, last_insert_rowid}`.

## The worker pool

Every statement runs on a thread from a small pool (`control-plane/worker.mjs`),
never on the main thread. This is not an optimisation — it is the difference
between one project's slow query costing that project and costing everyone.

`DatabaseSync` blocks the thread it runs on. When that was the process's only
thread, a slow query stopped the event loop, so other projects' requests were
not merely delayed — they were never read off the socket. Measured on the code
this replaced: while one project ran an ~8s query, an unrelated project's
`SELECT 1` waited 7.8s and then got a connection reset. With the pool, the same
unrelated query is answered in 12ms.

`QUERY_TIMEOUT_MS` (default 5000) bounds **the answer, not the query**. There is
no way to cancel a running statement: `DatabaseSync` exposes no interrupt, no
progress handler and no busy timeout, and SQLite's own `sqlite3_interrupt` is
not reachable from Node. On timeout the caller gets a 504 immediately and the
worker is terminated — but a native `sqlite3_step` already in flight runs to
completion before that thread actually dies. The pool spawns a replacement, so
capacity recovers either way.

`MAX_DB_BYTES` (default 50 MB) caps one project's file. Reads, `DELETE`, `DROP`
and `VACUUM` still work at the cap, deliberately: refusing them would leave a
full project with no statement it could run to get back under the limit.
`UPDATE` is treated as growth, since it can shrink or grow a row and there is no
way to know which beforehand.

## Backups

`backup.sh`, run nightly by `boxcode-db-backup.timer`, snapshots every project
file to `s3://boxcode-artifacts/backups/db/db-YYYY-MM-DD.tar.gz`. It uses
sqlite3's `.backup`, not `cp`: a byte copy of a database with a live journal
restores corrupt, and this runs against a live service. Restore instructions are
in the script's own footer.

## Known limitations
- No security hardening beyond the key check: the control-plane runs as
  root, and a valid key grants unrestricted SQL (including `DROP TABLE`)
  against that one project's file. Same "prove it works first" tradeoff
  auth's README documents.
- No backup existed at all before `backup.sh`; durability was "the EBS
  volume is still there". The nightly snapshot closes that, but there is
  still no point-in-time recovery — the most that can be lost is a day.
- SQLite's own concurrency limits (single-writer file locking) apply.
  Fine for the traffic this was built to prove out; a real concern if a
  project's data workload ever gets genuinely concurrent.
- No rate limiting on `/db/named-query`. It needs a valid `access_token`,
  but signing up for a project's own `enable_auth` is free and open to
  anyone, so a signed-in visitor can call a registered query as many times
  as they like. Fine for the single-developer, small-audience phase this
  is built for; worth revisiting alongside the same gap `/query` and
  `/uploads` already carry.
