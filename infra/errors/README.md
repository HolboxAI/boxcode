# boxcode runtime-error control-plane

A mailbox for what a published page's own browser reported about itself, not
a monitoring service. A developer publishes an artifact with
`publish_artifact`, adds the error beacon to their own page, and later opens
a boxcode session against that project -- which checks this mailbox and folds
any pending reports into what the model sees. There is no hosted agent here
and there will not be one, same principle as `infra/requests/`: interpreting
an error and actually fixing the code has to happen with the developer's own
LLM key, on their own machine, through the ordinary agent loop
(`src/errors.rs`, `list_reported_errors`/`resolve_reported_error` in
`src/tools.rs`). This control-plane only holds the note between "the page
threw" and "boxcode picked it up."

Reuses `auth.boxcode.sh`'s existing vhost and cert, same as `infra/db/` and
`infra/requests/` -- no new DNS or cert needed. Zero npm dependencies, same
stance as the other control-planes: `node:http`/`node:crypto`/`node:fs` are
all this needs.

## Why this is not just `infra/requests/` with a different field list

Two real differences from the change-request mailbox it otherwise mirrors,
both load-bearing:

- **Volume.** A human writes a change request once, on purpose. A browser
  writes an error report automatically, and a broken render loop can write
  thousands of them a minute. `infra/requests/` has no rate limit and no
  pruning because it has never needed one -- a human's own patience is the
  rate limit. This service needs a real one, so it has: a per-project cap
  (`RATE_LIMIT_PER_PROJECT`, default 60 per `RATE_WINDOW_MS`, default 5
  minutes) plus server-side dedup that collapses repeats of the exact same
  `(message, file, line, col)` into one record's `count` instead of storing
  each occurrence separately, plus a per-project record cap
  (`MAX_RECORDS_PER_PROJECT`, default 200, lowest-count-and-oldest pruned
  first) and a retention window (`RETENTION_DAYS`, default 30).

  The rate limit is deliberately **per-project**, not a single global cap
  like `infra/auth/`'s `GLOBAL_RATE`. That service's threat model is many
  distinct sources hammering one shared resource, where a global cap is the
  only thing that can't be escaped by renting more addresses. This service's
  threat model is the opposite: one project's own broken client code
  flooding itself. A global cap here would let one runaway project silence
  every other project's real errors by exhausting the whole host's shared
  budget -- so each project gets its own bucket, and one project misbehaving
  only ever costs that project's own mailbox.

- **Trust.** A change request sitting in the mailbox is inert text nobody
  acts on until a human reads it and decides. An error report is read
  *automatically* by the next boxcode session opened against that project
  and folded straight into model context. Accepting one against an
  unverified `project_id` means storing attacker-controlled text with an
  automatic, unattended reader -- not a mailbox with a human gatekeeper. So,
  unlike `infra/requests/`, this verifies `project_id` names a real, live,
  published artifact before accepting anything (`VERIFY_ARTIFACT`, same
  mechanism as `infra/auth/`'s `artifactExists()`: one `HEAD` against the
  public artifact URL, fails closed on any network error).

## What is deliberately NOT collected

`window.onerror`'s own signature is the entire allowlist: `message`, `file`,
`line`, `col`. Never a stack trace, never a thrown value's own properties,
never anything else. This is not "PII scrubbing" -- there is no general way
to scrub arbitrary application state, so this service does not try. It
avoids the problem by never collecting fields that could carry it. `file`
additionally has its query string stripped, both client-side (the beacon)
and again server-side (defense in depth): for an inline-script error, `file`
falls back to the page's own URL, and a query string on that can carry
session tokens or anything else the page put in its own address.

`message` and `file` are also truncated (500 characters each) -- shorter than
`infra/requests/`'s 4000-character text field, because this exists to
*identify* an error for a developer already looking at their own code, not
to hold a full report.

## Layout

- `control-plane/index.mjs` -- the one always-running process, on its own
  port (8083):
  - `GET /errors-beacon.js` -- a small, dependency-free vanilla JS beacon.
    Not generated per-project: one static file, and a developer adds it to
    their own published page with a single `<script src="https://
    auth.boxcode.sh/errors-beacon.js" data-project="<their artifact
    id>"></script>` tag via `edit_file`, then republishes -- there is no
    separate "enable" tool, same convention as `requests-widget.js`.
  - `POST /errors {project_id, message, file, line, col}` -- what the beacon
    submits, sent via `navigator.sendBeacon` where available (a
    `text/plain` `Blob`, which keeps this a CORS "simple request" so it is
    never preflighted, and is designed to survive page unload -- an error is
    sometimes the last thing a broken page does) or a `fetch(...,
    {keepalive: true})` fallback otherwise. Verifies `project_id` against
    the live artifact service, rate-limits per project, and dedupes against
    any existing pending record with the same `(message, file, line, col)`
    by incrementing its `count` rather than creating a new one.
  - `GET /errors?project_id=X[&status=all]` -- what the boxcode client
    polls. Pending only by default, oldest first.
  - `POST /errors/<id>/resolve {project_id}` -- what the boxcode client
    calls once it has acted on (or decided against) a report. Same
    ownership check as `infra/requests/`: a `project_id` that does not own
    the record gets the same 404 a nonexistent id would, never a 403 --
    this endpoint never confirms an id exists for a project the caller does
    not already know it belongs to. Idempotent.
- `control-plane/boxcode-errors-control-plane.service` -- the systemd unit.
- `setup.sh` -- adds this to a box that has already run
  `infra/auth/setup.sh`. Writes its nginx routes into
  `/etc/nginx/conf.d/auth-projects/_errors-route.conf` -- the directory the
  auth vhost already `include`s -- rather than touching
  `/etc/nginx/conf.d/auth.conf` itself, which certbot edits in place once
  TLS is set up; see `infra/db/setup.sh`'s own header for why overwriting
  that file from a template would be the mistake.

This is a new production service. `setup.sh` is meant to be run by hand on
the target box when ready, the same way the other control-planes' own
`setup.sh` scripts are -- nothing here deploys itself.

## CORS

`/errors-beacon.js` and `POST /errors` are called from the published
artifact page's own origin (a different origin than this control-plane), so
they carry `Access-Control-Allow-Origin: https://boxcode.sh` and `/errors`
answers its own `OPTIONS` preflight -- needed for the `fetch` fallback path;
the primary `sendBeacon` path avoids the preflight entirely by construction
(see above). `GET /errors` and `POST /errors/<id>/resolve` are called by the
boxcode CLI client directly, not a browser, so they carry no CORS headers --
same reasoning as `infra/requests/`'s equivalent endpoints.

## Known limitations

- No auth on submission beyond `project_id` naming a real, live artifact --
  and, sharper than that sounds: `artifactExists()` only proves the artifact
  is live, not that a given POST actually came from that artifact's own
  page. There is no Origin/Referer check anywhere in this file, and CORS
  constrains browser JS, not a direct `curl`/`sendBeacon`-equivalent call --
  so anyone who knows (or guesses, `PROJECT_ID_RE` is only 4-16
  lowercase-alnum chars) a project's PUBLIC artifact id can submit arbitrary
  `message` text against it directly, with no need to ever touch the real
  page. Bounded by the rate limit, dedup, and the fixed field allowlist (no
  stack, no arbitrary properties), but this is a real, open gap, not a
  theoretical one: unlike `infra/requests/` (a human reads a request before
  acting on it), `src/errors.rs` folds pending error reports straight into
  the model's context automatically the next time a boxcode session opens
  that project -- so this is a live prompt-injection channel, gated only by
  guessing a public id. Real authentication (a per-project submission token
  minted at publish time, say) is future work, deliberately not attempted in
  this pass.
- Published artifacts expire 48 hours after `publish_artifact`
  (`EXPIRY_HOURS` in `src/artifacts.rs`) unless republished. Error
  reporting only has a real window on artifacts the developer keeps
  republishing -- a one-off publish that lapses stops being verifiable by
  `artifactExists()`, and new reports against it will be refused.
- Same "prove it works first" posture as the other control-planes: this
  runs as root, and the store file has no encryption beyond its `0600`
  permissions.
- The per-project rate limit and dedup counters live in memory, not the
  store file -- a service restart resets them. Accepted deliberately, same
  reasoning as `infra/auth/`'s in-memory attempt counters: a file write on
  every submission attempt, including ones about to be rejected, would
  itself be a thing to abuse.
- The per-project rate limit is not itself hardened against the same
  no-real-auth gap above: since verification only proves a project id is
  real and live, not that the caller is the project's own page, anyone who
  knows a project's public artifact id (visible in its page URL) can submit
  enough distinct-looking junk to exhaust that project's rate-limit budget
  before its real errors arrive -- and, given `pruneIfNeeded` evicts the
  lowest-`count` records first, can eventually evict genuine one-off error
  reports too. Same root cause and same future fix (real per-project
  submission auth) as the injection gap above, not a separate problem.
- The store is now an in-memory cache flushed to disk at most once every
  `FLUSH_INTERVAL_MS` (default 1s), not written on every mutation -- added
  specifically so a flood of *repeated* identical reports (which cost
  nothing against the rate limit, by design) cannot each also force a full
  disk read+write inside the lock every other project's submissions queue
  behind. A mutation can be lost if the process is killed (not merely
  restarted -- shutdown handlers flush on `SIGTERM`/`SIGINT`) within that
  window. Accepted for the same reason as the in-memory rate-limit
  counters above: this is a mailbox for automatically-regenerated error
  reports, not a system of record -- a lost mutation just means the
  browser reports the same error again if the bug recurs.
