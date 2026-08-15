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
import { createServer } from "node:http";
import { DatabaseSync } from "node:sqlite";
import { readFile, writeFile, mkdir, chmod } from "node:fs/promises";
import path from "node:path";

const REGISTRY_PATH = process.env.REGISTRY_PATH || "/opt/boxcode-db/registry.json";
const DATA_DIR = process.env.DATA_DIR || "/opt/boxcode-db/data";
const PORT = Number(process.env.PORT || 8081);

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

// A prepared statement only ever runs one SQL statement -- unlike exec(),
// which accepts several semicolon-separated ones but returns nothing
// useful from any of them. One statement per call is the deliberate
// contract (see the DB_QUERY tool description in tools.rs); the model
// calls this tool again for a second statement rather than batching.
function isReadStatement(sql) {
  const head = sql.trim().slice(0, 10).toUpperCase();
  return head.startsWith("SELECT") || head.startsWith("PRAGMA") || head.startsWith("EXPLAIN");
}

function runQuery(projectId, sql, params) {
  const dbPath = path.join(DATA_DIR, `${projectId}.sqlite`);
  const db = new DatabaseSync(dbPath);
  try {
    const stmt = db.prepare(sql);
    if (isReadStatement(sql)) {
      const rows = stmt.all(...params);
      const truncated = rows.length > MAX_ROWS;
      return { rows: truncated ? rows.slice(0, MAX_ROWS) : rows, truncated };
    }
    const result = stmt.run(...params);
    return { changes: result.changes, last_insert_rowid: Number(result.lastInsertRowid) };
  } finally {
    db.close();
  }
}

const server = createServer(async (req, res) => {
  if (req.method !== "POST" || req.url !== "/query") {
    return fail(res, 404, "POST /query only");
  }

  let body = "";
  for await (const chunk of req) body += chunk;

  let parsed;
  try {
    parsed = JSON.parse(body || "{}");
  } catch {
    return fail(res, 400, "body is not JSON");
  }

  const { project_id: projectId, key, sql } = parsed;
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

  if (!(await authorize(projectId, key))) {
    return fail(res, 403, "key does not match this project's stored key");
  }

  try {
    await mkdir(DATA_DIR, { recursive: true });
    const result = runQuery(projectId, sql, params);
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify(result));
  } catch (e) {
    // A SQL error here is the ordinary, expected outcome of a bad
    // statement or a constraint violation, not a control-plane failure --
    // node:sqlite's own messages ("UNIQUE constraint failed: ...", "near
    // ...: syntax error") are already specific enough to hand back as-is
    // rather than wrapping them in something vaguer.
    fail(res, 400, e.message);
  }
});

server.listen(PORT, "127.0.0.1", () => {
  console.log(`boxcode db control-plane listening on 127.0.0.1:${PORT}`);
});
