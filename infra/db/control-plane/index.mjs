// boxcode db control-plane -- runs one SQL statement against one
// project's own SQLite file.
//
// Zero npm dependencies, same stance as boxcode-artifact-signer and the
// auth control-plane: `node:sqlite` is built into the Node runtime this
// box already has, so there is no driver to vendor. One file per project
// (/opt/boxcode-db/data/<project_id>.sqlite), created on first use --
// unlike GoTrue there is no per-project process or port here, because this
// isn't a fixed third-party server that needs its own instance for
// isolation; isolation comes from opening a different file per request.
//
// Every request must carry a key. It is never generated or seen here
// first -- the boxcode client generates and persists it locally
// (~/.boxcode/db.json, see src/db.rs), specifically so the key can never
// end up embedded in a published page's client-side JS by accident: the
// model that writes that JS never has it. The first request for a given
// project_id adopts whatever key it supplies as that project's key from
// then on (trust-on-first-use); every later request must match it
// exactly. See infra/db/README.md for why this, not open access by
// project id alone, and the known limitation TOFU carries.
//
// A request may also carry an access_token -- a project's own GoTrue
// token, the same one enable_auth's sign-in endpoint hands back (see
// src/auth.rs, infra/auth/). The key above proves which *project* a
// request belongs to; it says nothing about which of that project's own
// users is asking, so on its own it cannot back a query scoped to "my
// rows only" -- that would otherwise be enforced by nothing but a user id
// the page's own client-side JS supplies as a param, which any visitor to
// the live site can forge from devtools. verifyUser below spends one
// request against that project's own GoTrue (`AUTH_BASE`, the same box
// this control-plane runs on) to turn the token into a verified user id,
// bound into the query as the named parameter `:current_user_id` instead
// of trusted from the caller.
import { createServer } from "node:http";
import { DatabaseSync } from "node:sqlite";
import { readFile, writeFile, mkdir, chmod } from "node:fs/promises";
import path from "node:path";
import { Worker } from "node:worker_threads";
import { availableParallelism } from "node:os";
import { fileURLToPath } from "node:url";

const REGISTRY_PATH = process.env.REGISTRY_PATH || "/opt/boxcode-db/registry.json";
const DATA_DIR = process.env.DATA_DIR || "/opt/boxcode-db/data";
const PORT = Number(process.env.PORT || 8081);
// Loopback by default, exactly as this service has always bound: nginx on
// this same box is the only thing that has ever needed to reach it, and a
// service that starts listening on every interface because it was upgraded
// would be a security change nobody asked for.
//
// Set HOST=0.0.0.0 to also accept connections from inside the VPC. That is
// what boxcode-hosted backends need -- they run with no route to the public
// internet by design, so they cannot reach this service the way the agent
// does, via https://auth.boxcode.sh. Whatever can reach the port is then
// decided by the instance's security group, which is the right place for it.
const HOST = process.env.HOST || "127.0.0.1";

// How many statements may be in flight at once. Each occupies one OS thread
// blocked inside SQLite, so this is a thread count, not a request ceiling.
const POOL_SIZE = Number(process.env.POOL_SIZE || Math.max(2, Math.min(4, availableParallelism() - 1)));
// How long a caller waits before being told the query did not finish. Not a
// cancellation -- see worker.mjs on why SQLite cannot be interrupted from
// here. It bounds the *answer*, not the query.
const QUERY_TIMEOUT_MS = Number(process.env.QUERY_TIMEOUT_MS || 5000);
// Jobs waiting for a free worker. Past this the service says so rather than
// growing a queue nobody is still waiting on.
const MAX_QUEUE = Number(process.env.MAX_QUEUE || 64);
// Ceiling on one project's file. 0 disables. Reads and DELETEs still work at
// the cap; only statements that could grow it are refused.
const MAX_DB_BYTES = Number(process.env.MAX_DB_BYTES || 50 * 1024 * 1024);
// Same default as the auth control-plane's own AUTH_BASE (see
// infra/auth/control-plane/index.mjs) -- both run on the same box behind
// the same domain, and a project's auth is always reachable at
// `${AUTH_BASE}/${projectId}/`, the nginx route writeNginxConf there sets
// up. Overridable independently in case that ever stops being true.
const AUTH_BASE = process.env.AUTH_BASE || "https://auth.boxcode.sh";

// Same shape as the auth control-plane's PROJECT_ID_RE: an artifact id is
// how every project is identified everywhere in boxcode, so this has to
// accept exactly what that id looks like.
const PROJECT_ID_RE = /^[a-z2-9]{4,16}$/;
// The client generates this with the same charset/length conventions as
// boxcode-artifact-signer's own ids (see src/db.rs) -- checked here only
// to reject obviously-wrong input before it becomes a registry entry,
// not as a strength requirement enforced independently of the client.
const KEY_RE = /^[a-f0-9]{32,64}$/;

const MAX_ROWS = 500;

function fail(res, code, message) {
  res.writeHead(code, { "content-type": "application/json" });
  res.end(JSON.stringify({ error: message }));
}

async function loadRegistry() {
  try {
    return JSON.parse(await readFile(REGISTRY_PATH, "utf8"));
  } catch {
    return {};
  }
}

async function saveRegistry(registry) {
  await mkdir(path.dirname(REGISTRY_PATH), { recursive: true });
  await writeFile(REGISTRY_PATH, JSON.stringify(registry, null, 2), { mode: 0o600 });
  // See the auth control-plane's saveRegistry for why this is needed even
  // though `mode` is also passed above: it only takes effect when
  // writeFile creates the file, not when it overwrites one that already
  // existed under wider permissions.
  await chmod(REGISTRY_PATH, 0o600);
}

// Returns null if `key` is wrong for an already-known project_id;
// otherwise the registry, updated in memory (and persisted by the caller)
// to include this project_id if it was seen for the first time.
async function authorize(projectId, key) {
  const registry = await loadRegistry();
  const existing = registry[projectId];
  if (existing) {
    return existing.key === key ? registry : null;
  }
  registry[projectId] = { key, createdAt: new Date().toISOString() };
  await saveRegistry(registry);
  return registry;
}

// Name of the reserved table a developer's own agent-authenticated /query
// calls create and populate to expose a query to the live page -- an
// ordinary table in that project's own SQLite file, nothing this
// control-plane creates or manages itself. There is nothing new for the
// model to learn to register one: `CREATE TABLE IF NOT EXISTS
// __boxcode_named_queries__ (name TEXT PRIMARY KEY, sql TEXT NOT NULL)`
// then an INSERT, both ordinary db_query calls. See DB_QUERY's tool
// description in tools.rs for the exact contract handed to the model.
const NAMED_QUERIES_TABLE = "__boxcode_named_queries__";
const NAMED_QUERY_NAME_RE = /^[a-zA-Z_][a-zA-Z0-9_]{0,63}$/;

// A prepared statement only ever runs one SQL statement -- unlike exec(),
// which accepts several semicolon-separated ones but returns nothing
// useful from any of them. One statement per call is the deliberate
// contract (see the DB_QUERY tool description in tools.rs); the model
// calls this tool again for a second statement rather than batching.
function isReadStatement(sql) {
  const head = sql.trim().slice(0, 10).toUpperCase();
  return head.startsWith("SELECT") || head.startsWith("PRAGMA") || head.startsWith("EXPLAIN");
}

// Resolves to the caller's verified user id, or throws with a message
// meant to reach the model as-is (same style as runQuery's own SQL
// errors): a 401/network failure from GoTrue means the token is wrong or
// stale, not that this control-plane is broken, so it is not a 500.
//
// GoTrue's own `/user` endpoint (not something this repo owns -- see
// infra/auth/README.md) is the one already-correct place to ask "does this
// token identify a real, current session", the same check the auth
// control-plane itself never has to reimplement.
async function verifyUser(projectId, accessToken) {
  const url = `${AUTH_BASE}/${projectId}/user`;
  let response;
  try {
    response = await fetch(url, { headers: { authorization: `Bearer ${accessToken}` } });
  } catch (e) {
    throw new HttpError(401, `could not verify access_token: ${e.message}`);
  }
  if (!response.ok) {
    throw new HttpError(401, "access_token is invalid or expired");
  }
  const user = await response.json().catch(() => null);
  if (!user || typeof user.id !== "string" || user.id === "") {
    throw new HttpError(401, "access_token verified but returned no user id");
  }
  return user.id;
}

class HttpError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

// node:sqlite throws "Unknown named parameter" for any name in the bound
// object that the statement doesn't actually reference (confirmed live) --
// so a query that doesn't mention :current_user_id at all must never be
// handed the object, even when the caller sent a perfectly valid
// access_token "just in case". This is only ever a gate on *whether to
// bind*, never on *whether to verify*: verifyUser above still runs
// unconditionally whenever access_token is present, so a bad token is
// still rejected regardless of what a query happens to reference.
function referencesCurrentUserId(sql) {
  return /[:@$]current_user_id\b/.test(sql);
}

// ---- the SQLite pool -------------------------------------------------------
//
// Every statement this service runs goes through here, and the main thread
// never opens a database again. See worker.mjs for why: DatabaseSync blocks
// the thread it runs on, so doing this work inline meant one project's slow
// query stopped every other project from being served at all.

const WORKER_PATH = fileURLToPath(new URL("./worker.mjs", import.meta.url));

let nextJobId = 1;
const idle = [];
const queue = [];
// id -> { resolve, reject, timer, worker }
const inFlight = new Map();

function spawnWorker() {
  const worker = new Worker(WORKER_PATH, {
    workerData: {
      dataDir: DATA_DIR,
      maxRows: MAX_ROWS,
      namedQueriesTable: NAMED_QUERIES_TABLE,
      maxDbBytes: MAX_DB_BYTES,
    },
  });
  worker.on("message", (msg) => settle(worker, msg));
  // A worker that dies for any other reason (an OOM, a native crash) must not
  // strand the caller waiting on it, and must not shrink the pool.
  worker.on("error", (e) => settle(worker, { id: worker.jobId, ok: false, message: e.message }));
  worker.on("exit", () => {
    const i = idle.indexOf(worker);
    if (i !== -1) idle.splice(i, 1);
    if (!shuttingDown && pool.length < POOL_SIZE) replace(worker);
  });
  worker.unref();
  return worker;
}

let shuttingDown = false;
const pool = [];

function replace(dead) {
  const i = pool.indexOf(dead);
  if (i !== -1) pool.splice(i, 1);
  const fresh = spawnWorker();
  pool.push(fresh);
  idle.push(fresh);
  pump();
}

function settle(worker, msg) {
  if (!msg || msg.id === undefined) return;
  const entry = inFlight.get(msg.id);
  if (!entry) return; // already timed out; its answer is no longer wanted
  inFlight.delete(msg.id);
  clearTimeout(entry.timer);
  worker.jobId = undefined;
  idle.push(worker);
  if (msg.ok) {
    entry.resolve(msg.value);
  } else {
    entry.reject(
      msg.overCapacity ? new HttpError(413, msg.message) : new Error(msg.message)
    );
  }
  pump();
}

function pump() {
  while (queue.length > 0 && idle.length > 0) {
    const worker = idle.pop();
    const { job, resolve, reject } = queue.shift();
    worker.jobId = job.id;
    const timer = setTimeout(() => {
      // Give up on the answer and on the thread. Terminating cannot preempt a
      // native sqlite call already running -- worker.mjs explains why -- but
      // it does stop the thread the moment that call returns, and it frees the
      // caller now rather than whenever SQLite finishes.
      inFlight.delete(job.id);
      reject(new HttpError(504, `query exceeded ${QUERY_TIMEOUT_MS}ms and was abandoned`));
      worker.terminate();
    }, QUERY_TIMEOUT_MS);
    inFlight.set(job.id, { resolve, reject, timer, worker });
    worker.postMessage(job);
  }
}

function runOnPool(job) {
  return new Promise((resolve, reject) => {
    if (queue.length >= MAX_QUEUE) {
      return reject(new HttpError(503, "too many queries in flight; try again shortly"));
    }
    queue.push({ job: { ...job, id: nextJobId++ }, resolve, reject });
    pump();
  });
}

for (let i = 0; i < POOL_SIZE; i++) {
  const w = spawnWorker();
  pool.push(w);
  idle.push(w);
}

// `namedParams` is only ever `{ current_user_id }`, and only ever passed at
// all when the caller sent a verified access_token -- omitting it entirely for
// every other request (rather than passing `{}`) keeps today's
// access-token-less queries running through node:sqlite exactly as they did
// before this existed.
function runQuery(projectId, sql, params, namedParams) {
  return runOnPool({ op: "query", projectId, sql, params, namedParams });
}

async function readJsonBody(req) {
  let body = "";
  for await (const chunk of req) body += chunk;
  return JSON.parse(body || "{}");
}

async function handleQuery(req, res) {
  let parsed;
  try {
    parsed = await readJsonBody(req);
  } catch {
    return fail(res, 400, "body is not JSON");
  }

  const { project_id: projectId, key, sql, access_token: accessToken } = parsed;
  const params = Array.isArray(parsed.params) ? parsed.params : [];

  if (typeof projectId !== "string" || !PROJECT_ID_RE.test(projectId)) {
    return fail(res, 400, "project_id must look like a boxcode artifact id");
  }
  if (typeof key !== "string" || !KEY_RE.test(key)) {
    return fail(res, 400, "key must be a hex string boxcode's own client generated");
  }
  if (typeof sql !== "string" || sql.trim() === "") {
    return fail(res, 400, "sql must be a non-empty string");
  }
  if (accessToken !== undefined && (typeof accessToken !== "string" || accessToken === "")) {
    return fail(res, 400, "access_token must be a non-empty string if present");
  }

  if (!(await authorize(projectId, key))) {
    return fail(res, 403, "key does not match this project's stored key");
  }

  try {
    // Verified before the query runs, not fallen back on silently if it
    // fails: a caller that sent an access_token meant for it to matter, so
    // a bad one fails closed (401) rather than quietly running the query
    // with no :current_user_id bound, which would surface as a confusing
    // "missing named parameter" from node:sqlite instead of the real
    // reason.
    let namedParams;
    if (accessToken) {
      const userId = await verifyUser(projectId, accessToken);
      if (referencesCurrentUserId(sql)) namedParams = { current_user_id: userId };
    }

    await mkdir(DATA_DIR, { recursive: true });
    const result = await runQuery(projectId, sql, params, namedParams);
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify(result));
  } catch (e) {
    if (e instanceof HttpError) {
      return fail(res, e.status, e.message);
    }
    // A SQL error here is the ordinary, expected outcome of a bad
    // statement or a constraint violation, not a control-plane failure --
    // node:sqlite's own messages ("UNIQUE constraint failed: ...", "near
    // ...: syntax error") are already specific enough to hand back as-is
    // rather than wrapping them in something vaguer.
    fail(res, 400, e.message);
  }
}

// Looks up the SQL text registered under `name` for `projectId`. A missing
// table (nothing registered yet) and a missing row (wrong name) both resolve
// to `null` -- a public caller does not need to know which, only that there is
// nothing to run under that name.
function lookupNamedQuery(projectId, name) {
  return runOnPool({ op: "lookup", projectId, name });
}

// The client-facing counterpart to /query: reachable with nothing but a
// signed-in visitor's own access_token, no project key at all, because a
// key that authorized arbitrary SQL cannot safely be handed to a browser --
// see infra/db/README.md. What closes that gap without reopening it is
// that this route never accepts SQL from the caller, only a `name` -- the
// only statements it will ever run are ones the developer already wrote
// and registered themselves through the agent-authenticated /query route
// above. A visitor can only ever invoke a query their own developer chose
// to expose, never write one.
async function handleNamedQuery(req, res) {
  let parsed;
  try {
    parsed = await readJsonBody(req);
  } catch {
    return fail(res, 400, "body is not JSON");
  }

  const { project_id: projectId, access_token: accessToken, name } = parsed;
  const params = Array.isArray(parsed.params) ? parsed.params : [];

  if (typeof projectId !== "string" || !PROJECT_ID_RE.test(projectId)) {
    return fail(res, 400, "project_id must look like a boxcode artifact id");
  }
  // Required, not optional like /query's: this route exists specifically so
  // a signed-in visitor can reach their own data without the project's key
  // -- there is no anonymous path here, same stance infra/uploads/ already
  // takes and for the same reason.
  if (typeof accessToken !== "string" || accessToken === "") {
    return fail(res, 400, "access_token is required");
  }
  if (typeof name !== "string" || !NAMED_QUERY_NAME_RE.test(name)) {
    return fail(res, 400, `name must match ${NAMED_QUERY_NAME_RE}`);
  }

  try {
    const userId = await verifyUser(projectId, accessToken);

    const sql = await lookupNamedQuery(projectId, name);
    if (sql === null) {
      return fail(res, 404, `no named query "${name}" is registered for this project`);
    }

    // Not row-level security, same caveat /query's own :current_user_id
    // support already carries: this verifies *who's asking*, nothing stops
    // the registered SQL itself from ignoring current_user_id and reading
    // every row anyway. The developer who wrote and registered it still
    // has to get the WHERE clause right.
    const namedParams = referencesCurrentUserId(sql) ? { current_user_id: userId } : undefined;
    const result = await runQuery(projectId, sql, params, namedParams);
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify(result));
  } catch (e) {
    if (e instanceof HttpError) {
      return fail(res, e.status, e.message);
    }
    fail(res, 400, e.message);
  }
}

const server = createServer(async (req, res) => {
  if (req.method !== "POST") {
    return fail(res, 404, "POST only");
  }
  if (req.url === "/query") {
    return handleQuery(req, res);
  }
  if (req.url === "/named-query") {
    return handleNamedQuery(req, res);
  }
  return fail(res, 404, "POST /query or /named-query only");
});

server.listen(PORT, HOST, () => {
  console.log(
    `boxcode db control-plane listening on ${HOST}:${PORT} ` +
      `(pool ${POOL_SIZE}, timeout ${QUERY_TIMEOUT_MS}ms, cap ${Math.round(MAX_DB_BYTES / 1048576)}MB)`
  );
});
