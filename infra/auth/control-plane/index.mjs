// boxcode auth control-plane -- turns a project id into a running,
// isolated GoTrue instance.
//
// Zero npm dependencies, on purpose, matching boxcode-artifact-signer's own
// stance: this shells out to `psql`, `docker` and `nginx`, the same tools a
// human would reach for by hand, rather than owning client libraries for
// three different systems. It runs as a plain systemd-managed process on the
// box it provisions onto (not Lambda -- Postgres and Docker need a real,
// persistent filesystem and a long-lived daemon, which Lambda does not
// offer), listening on localhost only; nginx is what the internet reaches.
//
// One GoTrue container per project, each with its own Postgres database and
// its own JWT secret, so unrelated projects never share a user pool -- see
// infra/auth/README.md for why that split exists. Idempotent: calling
// /provision twice for the same project returns the same answer instead of
// doubling up.
import { createServer } from "node:http";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { randomBytes } from "node:crypto";
import { readFile, writeFile, mkdir, chmod } from "node:fs/promises";
import path from "node:path";

const run = promisify(execFile);

const REGISTRY_PATH = process.env.REGISTRY_PATH || "/opt/boxcode-auth/registry.json";
const NGINX_CONF_DIR = process.env.NGINX_CONF_DIR || "/etc/nginx/conf.d/auth-projects";
const SITE_BASE = process.env.SITE_BASE || "https://boxcode.sh";
// A real, owned domain, not this box's own AWS-assigned hostname -- that
// was tried first and Let's Encrypt refuses to issue for
// `*.compute.amazonaws.com` ("forbidden by policy"), and ACM cannot
// substitute either (same ownership requirement, plus its certs are not
// usable by plain nginx at all). See infra/auth/README.md and setup.sh's
// own header for the fuller explanation.
const AUTH_BASE = process.env.AUTH_BASE || "https://auth.boxcode.sh";
const PORT = Number(process.env.PORT || 8080);
const GOTRUE_PORT_BASE = 9000;
// Pinned, not `:latest` -- Supabase does not publish a `:latest` tag for
// this image at all ("manifest unknown", confirmed live), only versioned
// ones. v2.189.0 matches supabase/supabase's own reference
// docker-compose.yml as of this writing.
const GOTRUE_IMAGE = process.env.GOTRUE_IMAGE || "supabase/gotrue:v2.189.0";

// Same shape as an artifact id (see boxcode-artifact-signer's `ID_RE`):
// lowercase letters minus i/l/o (visually ambiguous) plus 2-9, 8 chars. A
// project id that does not look like one boxcode would have generated is
// refused outright -- it is about to become a Postgres database name, a
// Docker container name and an nginx filename, so "looks safe" has to be
// verified, not assumed.
const PROJECT_ID_RE = /^[a-z2-9]{4,16}$/;

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
  // `mode` above only applies when writeFile creates the file -- an
  // existing file (e.g. from before this file started holding secrets)
  // keeps its old, wider permissions unless explicitly chmod'd here too.
  await chmod(REGISTRY_PATH, 0o600);
}

function nextPort(registry) {
  const used = new Set(Object.values(registry).map((entry) => entry.port));
  let port = GOTRUE_PORT_BASE;
  while (used.has(port)) port += 1;
  return port;
}

async function databaseExists(name) {
  const { stdout } = await run("sudo", [
    "-u", "postgres", "psql", "-tAc",
    `SELECT 1 FROM pg_database WHERE datname = '${name}'`,
  ]);
  return stdout.trim() === "1";
}

async function createDatabase(name) {
  if (await databaseExists(name)) return;
  await run("sudo", ["-u", "postgres", "psql", "-c", `CREATE DATABASE ${name}`]);
}

async function containerIsRunning(name) {
  const { stdout } = await run("docker", ["ps", "-q", "-f", `name=^${name}$`]);
  return stdout.trim().length > 0;
}

async function removeContainerIfPresent(name) {
  try {
    await run("docker", ["rm", "-f", name]);
  } catch {
    // Nothing to remove -- fine, that's the common case.
  }
}

async function startGoTrue({ containerName, port, dbName, jwtSecret, siteUrl, apiExternalUrl }) {
  // A container that exists but is not running (crash-looping on bad
  // config, most often) is worse than no container: it satisfies "does
  // gotrue-<id> exist" checks forever without ever becoming reachable.
  // Recreating it, rather than only creating when totally absent, is what
  // makes a config fix on this file actually take effect on retry instead
  // of silently no-op'ing against the broken one.
  if (await containerIsRunning(containerName)) return;
  await removeContainerIfPresent(containerName);
  // --network host: simplest way for the container to reach Postgres on
  // 127.0.0.1 and for nginx on the same box to reach the container by port,
  // with no Docker network/DNS layer in between to debug. Acceptable on a
  // single trusted box; the first thing to revisit if this box ever stops
  // being single-tenant-per-project-trusted.
  await run("docker", [
    "run", "-d", "--network", "host", "--restart", "unless-stopped",
    "--name", containerName,
    "-e", `GOTRUE_API_HOST=127.0.0.1`,
    "-e", `GOTRUE_API_PORT=${port}`,
    "-e", `GOTRUE_DB_DRIVER=postgres`,
    "-e", `GOTRUE_DB_DATABASE_URL=postgres://postgres@127.0.0.1:5432/${dbName}`,
    "-e", `GOTRUE_JWT_SECRET=${jwtSecret}`,
    // Required for a login to actually issue a usable token -- confirmed
    // against Supabase's own reference docker-compose.yml, which sets this
    // unconditionally alongside GOTRUE_JWT_SECRET.
    "-e", `GOTRUE_JWT_AUD=authenticated`,
    "-e", `GOTRUE_JWT_EXP=3600`,
    "-e", `GOTRUE_SITE_URL=${siteUrl}`,
    "-e", `GOTRUE_URI_ALLOW_LIST=${SITE_BASE}`,
    // Required -- GoTrue refuses to start without it ("Failed to load
    // configuration: required key API_EXTERNAL_URL missing value",
    // confirmed live). This project's own auth_url, i.e. where GoTrue
    // itself is externally reachable, not SITE_URL (the published page).
    "-e", `API_EXTERNAL_URL=${apiExternalUrl}`,
    // No SMTP configured yet, so a signup that waited on a confirmation
    // email would never complete. Autoconfirm is the deliberate tradeoff for
    // "prove sign-up/sign-in works end to end" -- the first thing to revisit
    // once this needs to be real rather than a demo.
    "-e", `GOTRUE_MAILER_AUTOCONFIRM=true`,
    "-e", `GOTRUE_DISABLE_SIGNUP=false`,
    GOTRUE_IMAGE,
  ]);
}

async function writeNginxConf(id, port) {
  await mkdir(NGINX_CONF_DIR, { recursive: true });
  const confPath = path.join(NGINX_CONF_DIR, `${id}.conf`);
  // Trailing slash on proxy_pass is load-bearing: it strips the `/<id>`
  // location prefix before forwarding, so GoTrue sees `/signup`, not
  // `/<id>/signup`, without needing to know its own mount point.
  const conf = `location /${id}/ {\n    proxy_pass http://127.0.0.1:${port}/;\n    proxy_set_header Host $host;\n}\n`;
  await writeFile(confPath, conf);
  await run("nginx", ["-t"]);
  await run("nginx", ["-s", "reload"]);
}

// Self-healing, not just idempotent: an id already in the registry still
// runs the full sequence again rather than trusting the record blindly,
// because every step below is now cheap to repeat when already satisfied
// (containerIsRunning, databaseExists) and a real one is not -- a project
// whose container crash-looped on a bad config would otherwise stay
// silently broken behind a "successful" registry entry forever, with no
// way for a retry to ever reach the fix. Port and JWT secret are reused
// from the existing entry rather than regenerated, so re-running this for
// an already-healthy project is a true no-op: a fresh port would orphan
// the old nginx route, and a fresh secret would invalidate every session
// already issued.
async function provision(id) {
  const registry = await loadRegistry();
  const existing = registry[id];

  const port = existing?.port ?? nextPort(registry);
  const dbName = existing?.dbName ?? `proj_${id}`;
  const containerName = existing?.containerName ?? `gotrue-${id}`;
  const jwtSecret = existing?.jwtSecret ?? randomBytes(32).toString("hex");
  const siteUrl = `${SITE_BASE}/artifacts/${id}`;
  const apiExternalUrl = `${AUTH_BASE}/${id}`;

  await createDatabase(dbName);
  await startGoTrue({ containerName, port, dbName, jwtSecret, siteUrl, apiExternalUrl });
  await writeNginxConf(id, port);

  // Carries `jwtSecret` now, where it did not before -- needed so a
  // container recreated after a crash reuses the same signing key instead
  // of silently invalidating every session already issued. This file is
  // therefore secret-bearing; its permissions matter more than they did.
  registry[id] = {
    port, dbName, containerName, jwtSecret,
    createdAt: existing?.createdAt ?? new Date().toISOString(),
  };
  await saveRegistry(registry);

  return { auth_url: `${AUTH_BASE}/${id}` };
}

const server = createServer(async (req, res) => {
  if (req.method !== "POST" || req.url !== "/provision") {
    return fail(res, 404, "POST /provision only");
  }

  let body = "";
  for await (const chunk of req) body += chunk;

  let parsed;
  try {
    parsed = JSON.parse(body || "{}");
  } catch {
    return fail(res, 400, "body is not JSON");
  }

  const id = parsed.project_id;
  if (typeof id !== "string" || !PROJECT_ID_RE.test(id)) {
    return fail(res, 400, "project_id must look like a boxcode artifact id");
  }

  try {
    const result = await provision(id);
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify(result));
  } catch (e) {
    // Deliberately not attempting a rollback of whatever partially
    // succeeded (a database with no container, say): this is the "prove
    // the flow works" phase, and a human reading this message can finish
    // or clean up a rare failure by hand far more easily than a rollback
    // path that has to get every combination of partial failure right.
    console.error(`provision(${id}) failed:`, e);
    fail(res, 500, `provisioning failed: ${e.message}`);
  }
});

server.listen(PORT, "127.0.0.1", () => {
  console.log(`boxcode auth control-plane listening on 127.0.0.1:${PORT}`);
});
