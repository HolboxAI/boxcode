# boxcode auth control-plane

Gives a published boxcode artifact (`boxcode.sh/artifacts/<id>`) sign-up/
sign-in, without the user leaving boxcode or standing up their own Supabase/
Firebase project. Self-hosted [GoTrue](https://github.com/supabase/auth)
(Supabase's standalone, Apache-2.0 auth server) — one container per project,
each with its own Postgres database and JWT secret, so unrelated projects
never share a user pool.

Deliberately **not** the full Supabase stack: no PostgREST, Kong, Realtime
or Storage. PostgREST would duplicate the SQLite-per-project story planned
separately for general data access; Storage duplicates the S3 pipeline
`artifacts.rs`/`boxcode-artifact-signer` already provide; Kong is a gateway
for routing between several backend services, and there is only one here
(nginx does the per-project routing directly).

## Layout

- `control-plane/index.mjs` — the one always-running process. `POST
  /provision {"project_id": "<8-char artifact id>"}` creates that project's
  Postgres database, starts its GoTrue container, and writes the nginx
  route for it. Idempotent — calling it again for a project that already
  has a container just returns the same `auth_url`.
- `control-plane/boxcode-auth-control-plane.service` — the systemd unit.
- `nginx/auth.conf.template` — the base vhost for `auth.boxcode.sh`. This
  domain is not optional: an earlier version of this tried using the EC2
  instance's own AWS-assigned hostname instead (real and free, no DNS step
  needed), but Let's Encrypt refuses to issue for `*.compute.amazonaws.com`
  outright ("forbidden by policy") since anyone can get one of those for
  free, so it doesn't count as proof of ownership. AWS Certificate Manager
  doesn't substitute either -- same ownership requirement, and its certs
  can't be handed to a plain nginx process regardless. A real, owned domain
  is required, and `auth.boxcode.sh`'s DNS is at Namecheap (`boxcode.sh`
  is not in Route53) -- an A record pointing it at this box's IP has to
  exist before `setup.sh`'s certbot step will succeed. Per-project
  `location` blocks live in `/etc/nginx/conf.d/auth-projects/`, written by
  the control-plane service, never edited by hand.
- `setup.sh` — idempotent bootstrap for a fresh Amazon Linux 2023 box:
  Docker, Postgres, nginx + certbot, Node, and the control-plane service
  under systemd. Checks that `auth.boxcode.sh` actually resolves to the
  box before attempting the certbot step, with a clear message if not,
  rather than letting certbot's own error be the first sign of it.

## Deploying a change

No CI/CD yet — same as `boxcode-artifact-signer`'s Lambda, which also has
no pipeline. From a checkout of this repo, on the instance itself (via
Session Manager -- automated remote command execution against this box is
intentionally not something this project's tooling does on its own):

```
git pull && bash infra/auth/setup.sh
```

`setup.sh` is safe to re-run on a box that already has everything installed
— every step in it checks before it acts.

## What each project's GoTrue instance looks like

- `GOTRUE_SITE_URL` = that project's own `boxcode.sh/artifacts/<id>` URL.
- `GOTRUE_URI_ALLOW_LIST` = `https://boxcode.sh`, since the published page
  and the auth API are different origins (CORS).
- `GOTRUE_MAILER_AUTOCONFIRM=true` — no SMTP is configured, so a signup
  that waited on a confirmation email would never complete. This is the
  deliberate tradeoff for "prove sign-up/sign-in works end to end"; wiring
  up real email (and turning autoconfirm back off) is the natural next
  step once this needs to be more than a demo.
- Talks to Postgres over the loopback interface only — the control-plane
  service and every GoTrue container run with `--network host`, and
  Postgres is configured for peer auth on `local` connections only. Nothing
  here is reachable except through nginx.
- `auth_url` in every response is `https://auth.boxcode.sh/<id>`.

## What guards `/provision`

Provisioning starts a Docker container and a Postgres database on a shared
2 GB box, and a GoTrue container holds 30–50 MB. An endpoint that took nothing
but an id was therefore one that anyone could exhaust the machine with — around
forty `curl` commands, no volume required, taking auth, db, requests and uploads
down together, plus a Postgres database and container layer left behind
permanently for each one.

**There is deliberately no credential.** The only thing a caller can influence
is the id: port, database name, container name and JWT secret are all reused
from what that project already has, so re-provisioning someone else's restarts
their container with identical config and changes *nothing*. A key would have
been guarding a takeover the endpoint's own shape already makes impossible,
while adding a secret to distribute and a way for a real user to get locked out
of their own project.

What bounds the damage instead needs no idea who is asking. Four checks run
**before anything is created**, cheapest first:

1. **Global rate limit** — `GLOBAL_RATE` (default 20) provisions per hour
   across the whole host, from everyone combined. **This is the one that
   actually holds.** A per-source limit assumes the source is scarce, and it is
   not: cloud IPs are rentable by the thousand and a single IPv6 allocation
   hands one attacker 18 quintillion addresses, so against anyone distributing
   requests, per-source limiting is theatre. A global limit cannot be escaped
   by having more addresses because it does not care where a request came from.
2. **Per-source rate limit** — `RATE_LIMIT` (default 5) per hour, so one noisy
   client cannot spend everyone's global budget. A whole IPv6 **/64** counts as
   one source, not one address, for the reason above.
3. **The project cap** — `MAX_PROJECTS` (default 50). Not a policy limit: it is
   the number past which this box stops working, and it is the hard ceiling no
   volume of requests can push past. An *existing* project still re-provisions
   at the cap, because that is how a crash-looped container gets healed.
4. **The artifact must exist** — one `HEAD` against
   `https://boxcode.sh/artifacts/<id>`. This is what makes an id cost
   something: you cannot provision a project that was never published, so an
   attacker has to publish first, which is itself rate-limited, attributable
   and expiring. Fails closed — a network error means "cannot confirm".

Then `provision()` runs. Re-provisioning a healthy project is now genuinely
free: `startGoTrue` already returned early when the container was running, and
`writeNginxConf` now compares before writing, so the reload — the most
expensive thing on the path and the only part that gets worse as projects
accumulate — is skipped when nothing changed.

**The accepted cost:** a flood can consume the global budget and stop
legitimate provisions for the rest of the window. That is deliberate. A real
user waits; the alternative was the box running out of memory and taking four
services down with it.

## The reaper

An artifact expires after 48 hours, but until now its container, database and
nginx route stayed on this box forever — so container count only ever went up
and the box's memory was a countdown with no way to add time back.

An hourly sweep tears down projects whose artifact has stopped serving for
`REAP_AFTER_HOURS` (default 72 — deliberately longer than the artifact's own
48h, because republishing to the same id is ordinary and dropping auth the
moment a link lapsed would kill sessions on a project still being worked on).

A sweep, not a timer per project: idempotent, self-healing if a teardown half
fails, and one timer regardless of project count. One nginx reload for the
whole sweep, never on the request path.

## Known limitations (explicitly deferred, matching the "prove the flow
works first" goal this was built for)

- The control-plane service and every GoTrue container still run as root, and
  Postgres still has no password. `/provision` is no longer unauthenticated,
  but nothing else on this box has been hardened.
- No horizontal scaling story: everything is one box. A GoTrue container
  per project is cheap (tens of MB each), but this has not been load-
  tested past a handful of projects — which is why `MAX_PROJECTS` exists.
- The rate limiter is per-process and in memory, so a restart clears it and it
  does not span instances. Adequate while there is one box; `MAX_PROJECTS` and
  the artifact requirement are the durable controls, and neither is affected by
  a restart.
- Nothing stops someone publishing 50 artifacts and provisioning all of them to
  fill the cap. A credential would not have helped — they would use their own —
  and the damage is bounded to "no new projects until the reaper runs", not to
  the box falling over. Worth revisiting if it ever actually happens.
- No rollback on a partially-failed `/provision` call (e.g. the database
  gets created but the container fails to start) — `provision()` in
  `control-plane/index.mjs` says why, and a failure is logged with enough
  detail to finish or clean up by hand.
- No email delivery, so no real password-reset or email-verification flow
  yet — see `GOTRUE_MAILER_AUTOCONFIRM` above.
