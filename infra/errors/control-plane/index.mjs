// boxcode runtime-error control-plane -- a mailbox for what a published
// page's own browser reported about itself, not a monitoring service.
//
// Same shape as infra/requests/ (submit, poll, resolve; no hosted agent;
// the developer's own boxcode session is what actually reads these and
// fixes anything, via src/errors.rs). Two things do NOT transfer from that
// service, though, and both matter enough to be load-bearing here rather
// than left as "same as requests":
//
//   1. A human writes a change request once, on purpose. A browser writes
//      an error report automatically, and a broken render loop can write
//      thousands of them a minute. Submission here needs real dedup and a
//      real per-project rate limit; infra/requests/ needs neither.
//   2. A change-request id sitting in the mailbox is inert -- text nobody
//      acts on until a human reads it. An error report is read
//      automatically (by whichever boxcode session next opens this
//      project) and, per src/errors.rs, folded straight into what the
//      model sees. Accepting one from an unverified project id is
//      therefore accepting attacker-controlled text with an automatic
//      reader, not a mailbox with a human gatekeeper -- so, unlike
//      infra/requests/, this verifies the project against the live
//      artifact service before storing anything (same mechanism as
//      infra/auth/'s VERIFY_ARTIFACT).
//
// Zero npm dependencies, same stance as the auth/db/requests control-planes:
// `node:http`/`node:crypto`/`node:fs` are all this needs.
import { createServer } from "node:http";
import { randomUUID } from "node:crypto";
import { readFile, writeFile, mkdir, chmod } from "node:fs/promises";
import path from "node:path";

const STORE_PATH = process.env.STORE_PATH || "/opt/boxcode-errors/errors.json";
const PORT = Number(process.env.PORT || 8083);
// Published pages load the beacon from and submit reports to -- SITE_BASE in
// the auth control-plane, kept as its own env var here rather than importing
// that file, since this is a separate process with its own deploy story.
const ALLOWED_ORIGIN = process.env.ALLOWED_ORIGIN || "https://boxcode.sh";
// Where the beacon HEADs to confirm project_id names a real, live artifact --
// same URL shape infra/auth/'s artifactExists() uses.
const SITE_BASE = process.env.SITE_BASE || "https://boxcode.sh";

// Same shape as every other control-plane's PROJECT_ID_RE: an artifact id is
// how every project is identified everywhere in boxcode.
const PROJECT_ID_RE = /^[a-z2-9]{4,16}$/;

// window.onerror messages are occasionally huge (a whole minified bundle's
// worth of inlined JSON error payload, seen in the wild) -- shorter than
// infra/requests' MAX_TEXT_LENGTH (4000) because this is meant to identify an
// error, not hold a full report; the developer's own boxcode session reads
// the real file/line from the record, not a wall of message text.
const MAX_MESSAGE_LENGTH = 500;
const MAX_FILE_LENGTH = 500;

// Whether project_id has to name a real, live artifact before an error report
// against it is accepted. Unlike infra/requests/ (a human writes the text, so
// a wrong id just orphans one note nobody reads), an error report here is
// read *automatically* by the next boxcode session opened against that
// project and folded into model context (src/errors.rs) -- accepting one
// against an unverified id means storing attacker-controlled text with an
// automatic, unattended reader. Set VERIFY_ARTIFACT=0 only for local testing
// with no artifact service running.
const VERIFY_ARTIFACT = process.env.VERIFY_ARTIFACT !== "0";

// Per-project submission cap and window. A page with genuinely distinct bugs
// might report a handful of different errors in a session; tens of
// *distinct-looking* submissions in five minutes from one project is almost
// certainly a render loop generating slightly different messages/line
// numbers each time (a counter interpolated into the error text, say), not
// real bug diversity. This is separate from dedup below: dedup collapses
// exact repeats of the same error into one record's count; this caps total
// submission volume even when nothing collapses because each looks distinct.
//
// Deliberately per-project, not global like infra/auth's GLOBAL_RATE: that
// service's threat model is many distinct sources hammering one shared
// resource (provisioning), where a global cap is the only thing that can't
// be escaped by renting more addresses. This service's threat model is the
// opposite -- *one* project's own broken client code flooding itself -- so a
// global cap would let one runaway project silence every other project's
// real errors by exhausting the whole host's budget. Each project gets its
// own bucket instead; one project misbehaving costs that project's own
// mailbox, never anyone else's.
const RATE_LIMIT_PER_PROJECT = Number(process.env.RATE_LIMIT_PER_PROJECT || 60);
const RATE_WINDOW_MS = Number(process.env.RATE_WINDOW_MS || 5 * 60 * 1000);

// Retention: how many pending+resolved records one project may hold before
// the oldest, lowest-occurrence-count ones are pruned to make room, and how
// long a record survives regardless of count. infra/requests/ has neither --
// fine there, since a human only ever writes as many requests as they bother
// to type. An automatic reporter has no such ceiling on its own, so this
// service needs one explicitly.
const MAX_RECORDS_PER_PROJECT = Number(process.env.MAX_RECORDS_PER_PROJECT || 200);
const RETENTION_DAYS = Number(process.env.RETENTION_DAYS || 30);

function fail(res, code, message, extraHeaders = {}) {
  res.writeHead(code, { "content-type": "application/json", ...extraHeaders });
  res.end(JSON.stringify({ error: message }));
}

function corsHeaders() {
  return {
    "access-control-allow-origin": ALLOWED_ORIGIN,
    "access-control-allow-methods": "POST, GET, OPTIONS",
    "access-control-allow-headers": "content-type",
  };
}

async function loadStore() {
  try {
    return JSON.parse(await readFile(STORE_PATH, "utf8"));
  } catch {
    return {};
  }
}

async function saveStore(store) {
  await mkdir(path.dirname(STORE_PATH), { recursive: true });
  await writeFile(STORE_PATH, JSON.stringify(store, null, 2), { mode: 0o600 });
  // See infra/requests/'s saveStore for why this chmod is needed even though
  // `mode` is also passed above: it only applies when writeFile creates the
  // file, not when it overwrites one that already existed under wider
  // permissions.
  await chmod(STORE_PATH, 0o600);
}

// True when `id` names an artifact that is actually published and serving.
// Identical mechanism to infra/auth's artifactExists(): one HEAD against the
// public URL, fails closed on any network error, since accepting an error
// report on an unconfirmed id is exactly what this exists to prevent.
async function artifactExists(id) {
  if (!VERIFY_ARTIFACT) return true;
  try {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 5000);
    const res = await fetch(`${SITE_BASE}/artifacts/${id}`, {
      method: "HEAD",
      signal: controller.signal,
    });
    clearTimeout(timer);
    return res.ok;
  } catch {
    return false;
  }
}

// Per-project submission timestamps, in memory on purpose -- a restart
// clearing this is fine (worst case, one project gets a fresh window early),
// and a file would mean a disk write on every submission attempt, including
// ones about to be rejected, which is itself a thing to abuse.
const submissionTimes = new Map();

function rateLimited(projectId) {
  const now = Date.now();
  const seen = (submissionTimes.get(projectId) || []).filter((t) => now - t < RATE_WINDOW_MS);
  if (seen.length >= RATE_LIMIT_PER_PROJECT) {
    submissionTimes.set(projectId, seen);
    return true;
  }
  seen.push(now);
  submissionTimes.set(projectId, seen);
  // Bounded cleanup so a stream of distinct project ids cannot grow this map
  // without limit -- which would be its own denial of service.
  if (submissionTimes.size > 10000) {
    for (const [key, times] of submissionTimes) {
      if (times.every((t) => now - t >= RATE_WINDOW_MS)) submissionTimes.delete(key);
    }
  }
  return false;
}

// A stable point in the message/file/line/col that two reports of "the same"
// error should collapse onto. Message is included so two different errors
// that happen to share a file/line (rare, but real for a line that can throw
// in more than one way) do not merge into one.
function dedupeKey(entry) {
  return `${entry.message} ${entry.file} ${entry.line} ${entry.col}`;
}

// Strips a query string from a URL-shaped string, best-effort. `file` is
// frequently the page's own URL (inline-script errors have no separate
// script file), and a query string there can carry session tokens or other
// values the page interpolated into its own address -- exactly the kind of
// thing this service's fixed-field allowlist exists to keep out, so it is
// stripped again here, server-side, rather than trusting the beacon alone to
// have done it.
function stripQuery(value) {
  const q = value.indexOf("?");
  return q === -1 ? value : value.slice(0, q);
}

function truncate(value, max) {
  return value.length > max ? value.slice(0, max) : value;
}

async function pruneIfNeeded(store, projectId) {
  const entries = Object.values(store).filter((r) => r.project_id === projectId);
  if (entries.length <= MAX_RECORDS_PER_PROJECT) return;
  // Lowest occurrence count first, then oldest -- a record seen once months
  // ago is a better prune candidate than one seen a hundred times an hour
  // ago, even if the latter is numerically older by first_seen_at.
  entries.sort((a, b) => a.count - b.count || a.first_seen_at.localeCompare(b.first_seen_at));
  const toRemove = entries.slice(0, entries.length - MAX_RECORDS_PER_PROJECT);
  for (const entry of toRemove) delete store[entry.id];
}

function pruneExpired(store) {
  const cutoff = Date.now() - RETENTION_DAYS * 24 * 60 * 60 * 1000;
  for (const [id, entry] of Object.entries(store)) {
    const seen = Date.parse(entry.last_seen_at || entry.first_seen_at || 0);
    if (Number.isFinite(seen) && seen < cutoff) delete store[id];
  }
}

async function submit(projectId, message, file, line, col) {
  const store = await loadStore();
  pruneExpired(store);

  const key = dedupeKey({ message, file, line, col });
  const existing = Object.values(store).find(
    (r) => r.project_id === projectId && r.status === "pending" && dedupeKey(r) === key
  );
  if (existing) {
    existing.count += 1;
    existing.last_seen_at = new Date().toISOString();
    await saveStore(store);
    return { id: existing.id, deduped: true };
  }

  const id = randomUUID();
  const now = new Date().toISOString();
  store[id] = {
    id,
    project_id: projectId,
    message,
    file,
    line,
    col,
    count: 1,
    status: "pending",
    first_seen_at: now,
    last_seen_at: now,
  };
  await pruneIfNeeded(store, projectId);
  await saveStore(store);
  return { id, deduped: false };
}

async function listPending(projectId, includeAll) {
  const store = await loadStore();
  return Object.values(store)
    .filter((r) => r.project_id === projectId && (includeAll || r.status === "pending"))
    .sort((a, b) => a.first_seen_at.localeCompare(b.first_seen_at))
    .map((r) => ({
      id: r.id,
      message: r.message,
      file: r.file,
      line: r.line,
      col: r.col,
      count: r.count,
      first_seen_at: r.first_seen_at,
      last_seen_at: r.last_seen_at,
    }));
}

// Returns "resolved" | "not-found" | "wrong-project".
async function resolveError(id, projectId) {
  const store = await loadStore();
  const entry = store[id];
  if (!entry) return "not-found";
  if (entry.project_id !== projectId) return "wrong-project";
  if (entry.status !== "resolved") {
    entry.status = "resolved";
    entry.resolved_at = new Date().toISOString();
    await saveStore(store);
  }
  return "resolved";
}

// The beacon is generic and dependency-free on purpose, same stance as
// infra/requests/'s widget: not something an "enable" tool generates per
// project, one static file every published page can point at with its own
// data-project attribute. See src/tools.rs's tool schema description for how
// a developer wires it in with edit_file.
function beaconScript() {
  return `(function () {
  var script = document.currentScript;
  var projectId = script && script.getAttribute("data-project");
  if (!projectId) return;
  var apiBase = new URL(script.src).origin;
  var endpoint = apiBase + "/errors";

  function stripQuery(value) {
    var q = value.indexOf("?");
    return q === -1 ? value : value.slice(0, q);
  }

  function report(message, file, line, col) {
    var payload = {
      project_id: projectId,
      message: String(message == null ? "" : message).slice(0, 500),
      file: stripQuery(String(file || stripQuery(location.href))).slice(0, 500),
      line: Number(line) || 0,
      col: Number(col) || 0,
    };
    var body = JSON.stringify(payload);
    // sendBeacon with a text/plain Blob is a CORS "simple request" -- it
    // never gets preflighted, unlike a fetch with an explicit
    // application/json content-type -- and it is designed to survive page
    // unload, which matters here since an error is sometimes the last thing
    // a broken page ever does. Falls back to a keepalive fetch (still
    // best-effort on unload, and does need the server's CORS/OPTIONS
    // handling below) only where sendBeacon is unavailable.
    if (navigator.sendBeacon) {
      var blob = new Blob([body], { type: "text/plain" });
      navigator.sendBeacon(endpoint, blob);
      return;
    }
    fetch(endpoint, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: body,
      keepalive: true,
    }).catch(function () {});
  }

  // Never send anything beyond message/file/line/col -- no stack, no
  // arguments, no arbitrary object properties. This is deliberate: it is how
  // this beacon avoids needing a general PII scrubber, by simply never
  // collecting the fields that would carry it.
  window.addEventListener("error", function (event) {
    report(event.message, event.filename, event.lineno, event.colno);
  });
  window.addEventListener("unhandledrejection", function (event) {
    var reason = event.reason;
    var message = reason && reason.message ? reason.message : String(reason);
    report("Unhandled rejection: " + message, location.href, 0, 0);
  });
})();
`;
}

const server = createServer(async (req, res) => {
  const url = new URL(req.url, "http://localhost");

  if (req.method === "GET" && url.pathname === "/errors-beacon.js") {
    res.writeHead(200, { "content-type": "application/javascript; charset=utf-8" });
    res.end(beaconScript());
    return;
  }

  if (req.method === "OPTIONS" && url.pathname === "/errors") {
    res.writeHead(204, corsHeaders());
    res.end();
    return;
  }

  if (req.method === "POST" && url.pathname === "/errors") {
    let body = "";
    // sendBeacon's text/plain Blob arrives with no content-type this server
    // needs to branch on -- the body is JSON either way, whether it came
    // from sendBeacon or the fetch fallback.
    for await (const chunk of req) body += chunk;
    let parsed;
    try {
      parsed = JSON.parse(body || "{}");
    } catch {
      return fail(res, 400, "body is not JSON", corsHeaders());
    }

    const projectId = parsed.project_id;
    if (typeof projectId !== "string" || !PROJECT_ID_RE.test(projectId)) {
      return fail(res, 400, "project_id must look like a boxcode artifact id", corsHeaders());
    }
    const message = typeof parsed.message === "string" ? parsed.message.trim() : "";
    if (!message) {
      return fail(res, 400, "message must be a non-empty string", corsHeaders());
    }
    const file = typeof parsed.file === "string" ? stripQuery(parsed.file.trim()) : "";
    const line = Number.isFinite(parsed.line) ? Math.trunc(parsed.line) : 0;
    const col = Number.isFinite(parsed.col) ? Math.trunc(parsed.col) : 0;

    if (rateLimited(projectId)) {
      return fail(
        res,
        429,
        `this project is reporting errors faster than ${RATE_LIMIT_PER_PROJECT} per ` +
          `${Math.round(RATE_WINDOW_MS / 60000)} minutes; likely a loop, not distinct bugs`,
        corsHeaders()
      );
    }

    if (!(await artifactExists(projectId))) {
      return fail(res, 404, `no artifact is published at ${SITE_BASE}/artifacts/${projectId}`, corsHeaders());
    }

    const { id, deduped } = await submit(
      projectId,
      truncate(message, MAX_MESSAGE_LENGTH),
      truncate(file, MAX_FILE_LENGTH),
      line,
      col
    );
    res.writeHead(200, { "content-type": "application/json", ...corsHeaders() });
    res.end(JSON.stringify({ ok: true, id, deduped }));
    return;
  }

  if (req.method === "GET" && url.pathname === "/errors") {
    const projectId = url.searchParams.get("project_id");
    if (typeof projectId !== "string" || !PROJECT_ID_RE.test(projectId)) {
      return fail(res, 400, "project_id must look like a boxcode artifact id");
    }
    const includeAll = url.searchParams.get("status") === "all";
    const errors = await listPending(projectId, includeAll);
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify(errors));
    return;
  }

  const resolveMatch = req.method === "POST" && url.pathname.match(/^\/errors\/([^/]+)\/resolve$/);
  if (resolveMatch) {
    let body = "";
    for await (const chunk of req) body += chunk;
    let parsed;
    try {
      parsed = JSON.parse(body || "{}");
    } catch {
      return fail(res, 400, "body is not JSON");
    }
    const projectId = parsed.project_id;
    if (typeof projectId !== "string" || !PROJECT_ID_RE.test(projectId)) {
      return fail(res, 400, "project_id must look like a boxcode artifact id");
    }
    const outcome = await resolveError(resolveMatch[1], projectId);
    if (outcome === "not-found") return fail(res, 404, "no such error report");
    // A project id that does not own this record gets the same 404 a
    // nonexistent id would -- not a 403 -- so this endpoint never confirms
    // that a given id exists for a project the caller does not already know
    // it belongs to. Same convention as infra/requests/.
    if (outcome === "wrong-project") return fail(res, 404, "no such error report");
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ ok: true }));
    return;
  }

  fail(res, 404, "no such route");
});

server.listen(PORT, "127.0.0.1", () => {
  console.log(`boxcode errors control-plane listening on 127.0.0.1:${PORT}`);
});
