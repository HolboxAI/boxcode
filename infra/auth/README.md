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
- `nginx/auth.conf.template` — the base vhost. No custom domain: `boxcode.sh`
  turned out to be managed at the registrar (Namecheap), not Route53, and
  the published page needs an HTTPS origin to call regardless (mixed
  content is blocked outright), so this uses the EC2 instance's own
  AWS-assigned public hostname instead — real, publicly resolvable, free,
  and available the instant the instance launches, no DNS step by anyone.
  `setup.sh` substitutes it in and gets a Let's Encrypt cert for it. Per-
  project `location` blocks live in `/etc/nginx/conf.d/auth-projects/`,
  written by the control-plane service, never edited by hand. If this ever
  gets a real subdomain later, that's a DNS change plus a `certbot --nginx
  -d <domain>` re-run, nothing here needs to change to support it.
- `setup.sh` — idempotent bootstrap for a fresh Amazon Linux 2023 box:
  Docker, Postgres, nginx + certbot, Node, and the control-plane service
  under systemd.

## Deploying a change

No CI/CD yet — same as `boxcode-artifact-signer`'s Lambda, which also has
no pipeline. From a checkout of this repo, with SSM access to the instance:

```
aws ssm send-command \
  --instance-ids <instance-id> \
  --document-name AWS-RunShellScript \
  --parameters commands='["cd /path/to/boxcode && git pull && bash infra/auth/setup.sh"]'
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
- `auth_url` in every response is `https://<this box's public hostname>/<id>`
  — no custom domain, see the `nginx/auth.conf.template` note above.

## Known limitations (explicitly deferred, matching the "prove the flow
works first" goal this was built for)

- No security hardening: the control-plane service and every GoTrue
  container run as root, Postgres has no password, and `/provision` itself
  has no auth of its own beyond "the id looks like a real artifact id".
- No horizontal scaling story: everything is one box. A GoTrue container
  per project is cheap (tens of MB each), but this has not been load-
  tested past a handful of projects.
- No rollback on a partially-failed `/provision` call (e.g. the database
  gets created but the container fails to start) — `provision()` in
  `control-plane/index.mjs` says why, and a failure is logged with enough
  detail to finish or clean up by hand.
- No email delivery, so no real password-reset or email-verification flow
  yet — see `GOTRUE_MAILER_AUTOCONFIRM` above.
