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
import { readFile, writeFile, mkdir, chmod, rename } from "node:fs/promises";
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

// The submitting beacon is fire-and-forget (sendBeacon never sees the
// response) and artifact verification fails closed, so a silently-broken
// dependency (the artifact service down, say) would otherwise drop every
// submission from every project with nothing anywhere to notice by. This is
// deliberately not a metrics/logging library -- a periodic summary line on
// this process's own stdout, which is all a zero-dependency service like
// this one needs to make an extended outage visible instead of invisible.
const rejectionCounts = { badRequest: 0, notFound: 0, rateLimited: 0 };
function trackRejection(kind) {
  rejectionCounts[kind] = (rejectionCounts[kind] || 0) + 1;
}
const REJECTION_LOG_INTERVAL_MS = 5 * 60 * 1000;
setInterval(() => {
  const { badRequest, notFound, rateLimited } = rejectionCounts;
  const total = badRequest + notFound + rateLimited;
  if (total === 0) return;
  console.log(
    `[errors] rejected ${total} submission(s) in the last ` +
      `${Math.round(REJECTION_LOG_INTERVAL_MS / 60000)}m ` +
      `(400:${badRequest} 404:${notFound} 429:${rateLimited})`
  );
  rejectionCounts.badRequest = 0;
  rejectionCounts.notFound = 0;
  rejectionCounts.rateLimited = 0;
  // unref() so this interval alone can never keep the process alive -- it's
  // an observability aid, not a reason to stay up.
}, REJECTION_LOG_INTERVAL_MS).unref();

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

// In-memory cache of the store, kept warm for the process lifetime, plus a
// dirty flag and a periodic flush (below) instead of a disk write on every
// mutation. Added because the dedup path -- the one this whole file exists
// to make cheap -- still did a full disk read+write per repeat even after
// dedup stopped charging the rate limit: proven the hard way, a flood of 500
// identical submissions from one project (all accepted, since repeats are
// free) each did a full loadStore/JSON.stringify(whole store)/writeFile/
// rename inside the SAME global withStoreLock every other project's
// submissions queue behind, and a concurrent, unrelated project's single
// genuine error sat blocked for 12+ seconds behind that queue. Real bugs
// don't repeat identically thousands of times a minute; a broken render
// loop does, and that is exactly the traffic this coalesces.
//
// Correctness still comes from withStoreLock, unchanged: every mutation
// below still runs inside it, so the "read, decide, mutate" sequence for two
// concurrent submissions can never interleave (proven: two concurrent
// submissions of the same new error produce count 2, never 1, across every
// probe). What changed is what happens to disk, not who is allowed to touch
// `cachedStore` at once -- mutations flip a dirty flag instead of writing,
// and a single periodic timer (also inside the lock, so it can never race a
// mutation either) does the actual write, at most once per FLUSH_INTERVAL_MS
// regardless of how many mutations happened in between.
//
// Tradeoff, stated plainly: a mutation can be lost if the process is killed
// (not merely restarted -- see the SIGTERM/SIGINT flush below) between the
// mutation and the next flush, a window of at most FLUSH_INTERVAL_MS. For an
// automatically-regenerated error report (the browser will just report it
// again if the bug recurs) this is the same "a restart clears it, worst case
// one project gets a fresh window early" tradeoff this file already accepts
// for the in-memory rate-limiter state above -- not a new risk class.
let cachedStore = null;
let loadPromise = null;
let storeDirty = false;

// Guards against two concurrent callers both missing the cache before either
// has finished the first load and each independently loading (and one
// silently clobbering the other's reference). Reachable even though
// submit()/resolveError() already serialize via withStoreLock, because
// listPending() (a plain GET) reads the cache WITHOUT the lock -- a GET
// racing the very first POST/resolve at process startup is the case this
// closes.
async function getStore() {
  if (cachedStore) return cachedStore;
  if (!loadPromise) {
    loadPromise = loadStore().then((s) => {
      cachedStore = s;
      return s;
    });
  }
  return loadPromise;
}

function markDirty() {
  storeDirty = true;
}

// Persists `cachedStore` if (and only if) something has mutated it since the
// last flush. Clears the dirty flag BEFORE writing, not after: any mutation
// that lands during the write's own awaits (mkdir/writeFile/rename) sets the
// flag true again on its own, synchronously, before this function's `await
// saveStore` resumes -- so a write-in-flight never causes a later mutation
// to be silently skipped, it just waits for the next tick. If the write
// itself fails, the flag is restored so the next periodic tick retries
// rather than treating a failed persist as done.
async function flushIfDirty() {
  if (!storeDirty) return;
  storeDirty = false;
  try {
    await saveStore(cachedStore);
  } catch (err) {
    storeDirty = true;
    throw err;
  }
}

// Bounds how stale the on-disk copy can be, independent of submission
// volume -- the entire point of decoupling persistence from the request
// path. Runs inside withStoreLock so it can never race a mutation's own
// read-modify-write against the same `cachedStore` object.
const FLUSH_INTERVAL_MS = Number(process.env.FLUSH_INTERVAL_MS || 1000);
setInterval(() => {
  withStoreLock(flushIfDirty).catch((err) => {
    console.error("[errors] periodic flush failed:", err && err.message);
  });
}, FLUSH_INTERVAL_MS).unref();

// pruneExpired used to run inside submit() on every single call -- a full
// O(store size) scan of every record for every project, paid by every
// submission including dedup hits that otherwise cost nothing. Harmless
// against a small store, but it was the next bottleneck uncovered once the
// per-submission disk write above was removed: a flood of 500 identical
// submissions against a realistic 10k-record store dropped from 12s (disk
// I/O in the lock) to ~1.25s (this scan, still in the lock) before moving it
// here. Expiry does not need per-request precision -- a record that is 30
// days old does not meaningfully change if it is actually pruned this
// second or up to a minute from now -- so it runs on its own, much less
// frequent timer instead, still inside withStoreLock so it can never race a
// submission's own read-modify-write.
const PRUNE_INTERVAL_MS = Number(process.env.PRUNE_INTERVAL_MS || 60 * 1000);
setInterval(() => {
  withStoreLock(async () => {
    const store = await getStore();
    const before = Object.keys(store).length;
    pruneExpired(store);
    if (Object.keys(store).length !== before) markDirty();
  }).catch((err) => {
    console.error("[errors] periodic prune failed:", err && err.message);
  });
}, PRUNE_INTERVAL_MS).unref();

// Best-effort final flush on an ordinary shutdown (not a crash -- nothing
// can catch that) so a plain restart/redeploy does not lose whatever landed
// in the last FLUSH_INTERVAL_MS.
for (const sig of ["SIGTERM", "SIGINT"]) {
  process.on(sig, () => {
    withStoreLock(flushIfDirty)
      .catch((err) => console.error("[errors] shutdown flush failed:", err && err.message))
      .finally(() => process.exit(0));
  });
}

// Writes are atomic (write to a sibling temp file, then rename() over the
// real path -- rename is atomic on the same filesystem, a direct overwrite is
// not) AND serialized (see withStoreLock below). Atomicity alone is not
// enough: two concurrent load-modify-save cycles can still race each other
// and each overwrite the other's changes even if each individual save() is
// itself atomic. Both together are what a concurrent flood of distinct
// submissions actually needs -- proven the hard way: an earlier version of
// this file did a bare `writeFile` with no rename and no lock, and 40
// concurrent distinct submissions corrupted the store file entirely (the
// interleaved writes produced unparseable JSON), which loadStore's `catch`
// then silently treated as an empty store on the very next write, destroying
// every previously-stored report for every project on the host.
async function saveStore(store) {
  const dir = path.dirname(STORE_PATH);
  await mkdir(dir, { recursive: true });
  const tmpPath = path.join(dir, `.errors.json.tmp.${process.pid}.${randomUUID()}`);
  await writeFile(tmpPath, JSON.stringify(store, null, 2), { mode: 0o600 });
  // See infra/requests/'s saveStore for why this chmod is needed even though
  // `mode` is also passed above: it only applies when writeFile creates the
  // file, not when it overwrites one that already existed under wider
  // permissions.
  await chmod(tmpPath, 0o600);
  await rename(tmpPath, STORE_PATH);
}

// Serializes every load-modify-save cycle against the store file so
// concurrent requests never interleave. A single in-process promise chain is
// enough for a single-process Node service -- no external lock or database
// needed. `withStoreLock` always advances the queue (via `.catch(() => {})`)
// even when `fn` throws, so one failed operation can never wedge every
// operation after it; the caller still sees the real rejection via the
// returned promise, only the internal queue-advancement swallows it.
let storeQueue = Promise.resolve();
function withStoreLock(fn) {
  const result = storeQueue.then(fn, fn);
  storeQueue = result.then(
    () => {},
    () => {}
  );
  return result;
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

// Returns `{ id, deduped }` on success or `{ rateLimited: true }` if a
// genuinely new record was refused for exceeding the per-project budget.
//
// Dedup runs BEFORE the rate-limit check, and only a genuinely-new record
// charges the rate limit -- deliberately, not incidentally. Repeated reports
// of the exact same bug (the traffic pattern this service exists to absorb)
// collapse into one record's count and cost nothing against the budget, so
// they can never crowd out a later, genuinely distinct error from the same
// project. The whole load-dedup-ratelimit-mutate sequence runs inside
// withStoreLock so a burst of concurrent identical submissions cannot each
// see "no existing record yet" and each create their own. Persistence is
// separate (see getStore/markDirty/flushIfDirty above): a mutation here
// marks the in-memory store dirty rather than writing it, so a flood of
// dedup hits costs no disk I/O at all, only the periodic flush does.
async function submit(projectId, message, file, line, col) {
  return withStoreLock(async () => {
    const store = await getStore();

    const key = dedupeKey({ message, file, line, col });
    const existing = Object.values(store).find(
      (r) => r.project_id === projectId && r.status === "pending" && dedupeKey(r) === key
    );
    if (existing) {
      existing.count += 1;
      existing.last_seen_at = new Date().toISOString();
      markDirty();
      return { id: existing.id, deduped: true };
    }

    if (rateLimited(projectId)) {
      return { rateLimited: true };
    }

    const id = `err_${randomUUID()}`;
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
    markDirty();
    return { id, deduped: false };
  });
}

// Reads the same in-memory cache submit()/resolveError() mutate, not a fresh
// disk load -- so a GET reflects the latest count/status even when it
// hasn't been flushed to disk yet. Deliberately not wrapped in
// withStoreLock: this only reads (filter/sort/map, no mutation), and every
// mutation elsewhere is itself synchronous once it has the store reference,
// so there is no half-mutated state for a concurrent read to observe.
async function listPending(projectId, includeAll) {
  const store = await getStore();
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

// Returns "resolved" | "not-found" | "wrong-project". Goes through the same
// withStoreLock as submit() -- both mutate the same in-memory store, and a
// resolve racing a submit (or another resolve) is exactly the interleaving
// that made the store corruptible before the lock existed. Marks dirty
// rather than saving immediately, same reasoning as submit() -- resolve is
// inherently low-frequency (a human action), so this isn't where the
// flood-of-writes problem lives, and giving it a separate immediate-save
// path would only add a second persistence mechanism to reason about for no
// real benefit: it is still bounded by the same FLUSH_INTERVAL_MS, and a
// graceful shutdown flushes it same as any other pending mutation.
async function resolveError(id, projectId) {
  return withStoreLock(async () => {
    const store = await getStore();
    const entry = store[id];
    if (!entry) return "not-found";
    if (entry.project_id !== projectId) return "wrong-project";
    if (entry.status !== "resolved") {
      entry.status = "resolved";
      entry.resolved_at = new Date().toISOString();
      markDirty();
    }
    return "resolved";
  });
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

// The whole body runs inside one try/catch: an uncaught exception in an
// async request listener (a disk I/O failure from saveStore's
// mkdir/writeFile/rename, say -- disk full, a permissions change, the store
// directory removed out from under it) would otherwise be an unhandled
// promise rejection that crashes the entire process, taking down every
// other project's requests along with the one that failed. A submission
// failing with a clean 500 is recoverable (the beacon gets no response
// either way, fire-and-forget); the whole service going down is not.
const server = createServer(async (req, res) => {
  try {
    await handleRequest(req, res);
  } catch (err) {
    console.error("[errors] request handler threw:", err && err.message);
    if (!res.headersSent) {
      fail(res, 500, "internal error", corsHeaders());
    } else {
      res.end();
    }
  }
});

async function handleRequest(req, res) {
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
      trackRejection("badRequest");
      return fail(res, 400, "body is not JSON", corsHeaders());
    }

    const projectId = parsed.project_id;
    if (typeof projectId !== "string" || !PROJECT_ID_RE.test(projectId)) {
      trackRejection("badRequest");
      return fail(res, 400, "project_id must look like a boxcode artifact id", corsHeaders());
    }
    const message = typeof parsed.message === "string" ? parsed.message.trim() : "";
    if (!message) {
      trackRejection("badRequest");
      return fail(res, 400, "message must be a non-empty string", corsHeaders());
    }
    const file = typeof parsed.file === "string" ? stripQuery(parsed.file.trim()) : "";
    // Clamped non-negative: the Rust side (src/errors.rs) declares line/col
    // as unsigned, so a negative value here would fail to deserialize on
    // that end -- silently, since an unconfigured-vs-unreachable distinction
    // swallows the resulting error. Clamp rather than reject outright: a
    // malformed line/col from a weird browser is still a real error worth
    // recording, just not at a nonsensical position.
    const line = Number.isFinite(parsed.line) ? Math.max(0, Math.trunc(parsed.line)) : 0;
    const col = Number.isFinite(parsed.col) ? Math.max(0, Math.trunc(parsed.col)) : 0;

    // Artifact verification runs before anything that touches the rate
    // limiter or the store, deliberately: rate-limiting first (as an earlier
    // version of this file did) let anyone who knows a victim's PUBLIC
    // artifact id (it's in the page URL) burn that project's rate-limit
    // budget with junk POSTs before its real errors ever arrive, without
    // needing to pass verification at all -- a cross-project DoS costing the
    // attacker nothing. Checking verification first means an attacker still
    // needs a real, live artifact id to affect anything.
    if (!(await artifactExists(projectId))) {
      trackRejection("notFound");
      return fail(res, 404, `no artifact is published at ${SITE_BASE}/artifacts/${projectId}`, corsHeaders());
    }

    const result = await submit(
      projectId,
      truncate(message, MAX_MESSAGE_LENGTH),
      truncate(file, MAX_FILE_LENGTH),
      line,
      col
    );
    if (result.rateLimited) {
      trackRejection("rateLimited");
      return fail(
        res,
        429,
        `this project is reporting genuinely new-looking errors faster than ` +
          `${RATE_LIMIT_PER_PROJECT} per ${Math.round(RATE_WINDOW_MS / 60000)} minutes; ` +
          `likely a loop, not distinct bugs (exact repeats of an already-seen error are ` +
          `deduped and never count against this)`,
        corsHeaders()
      );
    }
    res.writeHead(200, { "content-type": "application/json", ...corsHeaders() });
    res.end(JSON.stringify({ ok: true, id: result.id, deduped: result.deduped }));
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
}

server.listen(PORT, "127.0.0.1", () => {
  console.log(`boxcode errors control-plane listening on 127.0.0.1:${PORT}`);
});
