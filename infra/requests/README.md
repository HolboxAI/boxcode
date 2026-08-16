# boxcode change-request control-plane

A mailbox, not an editor. A developer publishes an artifact with
`publish_artifact`, then checks it later -- sometimes from a phone, hours
after publishing -- and wants to leave a plain-English request ("move the
search button right") without running boxcode themselves. There is no
hosted agent here and there will not be one: interpreting that sentence and
actually editing code has to happen with the developer's own LLM key, on
their own machine, through the ordinary agent loop (`src/requests.rs`,
`list_change_requests`/`resolve_change_request` in `src/tools.rs`). This
control-plane only holds the note between "someone left it" and "boxcode
picked it up."

Reuses `auth.boxcode.sh`'s existing vhost and cert, same as `infra/db/` --
no new DNS or cert needed. Zero npm dependencies, same stance as the auth
and db control-planes: `node:http`/`node:crypto`/`node:fs` are all this
needs, so unlike `infra/db/` it runs fine on this box's system node (no
`node:sqlite`, no Node 22.5+ requirement).

## Layout

- `control-plane/index.mjs` -- the one always-running process, on its own
  port (8082):
  - `GET /requests-widget.js` -- a small, dependency-free vanilla JS widget
    (floating "Request a change" button + textarea). Not generated
    per-project: it is one static file, and a developer adds it to their
    own published page with a single `<script src="https://auth.boxcode.sh
    /requests-widget.js" data-project="<their artifact id>"></script>` tag
    via `edit_file`, then republishes -- there is no separate "enable" tool.
  - `POST /requests {project_id, text}` -- what the widget submits. Stores
    `{id, project_id, text, status, created_at}` in a JSON file, same
    read-modify-write shape as the auth control-plane's `registry.json`.
  - `GET /requests?project_id=X[&status=all]` -- what the boxcode client
    polls. Pending only by default, oldest first.
  - `POST /requests/<id>/resolve {project_id}` -- what the boxcode client
    calls once it has acted on (or decided against) a request. The
    `project_id` must match the request's own, or this returns the same 404
    a nonexistent id would -- it never confirms a request id exists for a
    project the caller does not already know it belongs to. Idempotent.
- `control-plane/boxcode-requests-control-plane.service` -- the systemd
  unit.
- `setup.sh` -- adds this to a box that has already run
  `infra/auth/setup.sh`. Writes its nginx routes into
  `/etc/nginx/conf.d/auth-projects/_requests-route.conf` -- the directory
  the auth vhost already `include`s -- rather than touching
  `/etc/nginx/conf.d/auth.conf` itself, which certbot edits in place once
  TLS is set up; see `infra/db/setup.sh`'s own header for why overwriting
  that file from a template would be the mistake.

## CORS

`/requests-widget.js` and `POST /requests` are called from the published
artifact page's own origin (a different origin than this control-plane), so
they carry `Access-Control-Allow-Origin: https://boxcode.sh` and `/requests`
answers its own `OPTIONS` preflight. `GET /requests` and
`POST /requests/<id>/resolve` are called by the boxcode CLI client directly,
not a browser, so they carry no CORS headers -- same reasoning as
`/provision` and `/db/query`.

## Known limitations

- No auth on submission beyond `project_id` looking like a real artifact
  id -- anyone who knows (or guesses) a project id can leave it a request.
  Low stakes on its own (a request is just text sitting in a mailbox until
  a human reads it and decides whether to act), but worth knowing.
- No rate-limiting. A script could flood a project's mailbox with
  submissions; nothing here throttles that yet.
- No widget theming/customization -- one fixed look, bottom-right corner.
- Same "prove it works first" posture as `infra/auth/` and `infra/db/`:
  the control-plane runs as root, and the store file has no encryption
  beyond its `0600` permissions.
