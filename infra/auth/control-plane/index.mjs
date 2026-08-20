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
import { readFile, writeFile, mkdir, chmod, rm } from "node:fs/promises";
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
// The ceiling on live projects. Every one of them is a Docker container
// holding 30-50MB and a Postgres database, on a box with 2GB of RAM -- so
// this is not a policy limit, it is the number past which the machine
// stops working. Deliberately well under that.
const MAX_PROJECTS = Number(process.env.MAX_PROJECTS || 50);

// Provisions one source may ask for per window, and how long the window is.
// Provisioning is expensive and rare -- a project is provisioned once and
// then re-provisioned only to self-heal -- so this can be strict without
// ever inconveniencing a real user.
const RATE_LIMIT = Number(process.env.RATE_LIMIT || 5);
const RATE_WINDOW_MS = Number(process.env.RATE_WINDOW_MS || 60 * 60 * 1000);

// Provisions the whole host will perform per window, from all sources
// combined.
//
// This is the one that actually holds. A per-source limit assumes the source
// is scarce, and it is not: cloud IPs are rentable by the thousand, and a
// single IPv6 allocation hands one attacker 18 quintillion addresses. Against
// anyone distributing their requests, per-source limiting is theatre.
//
// A global limit cannot be escaped by having more addresses, because it does
// not care where a request came from. The cost is that a flood can consume
// the budget and stop legitimate provisions for the rest of the window --
// accepted deliberately: a real user waits, where the alternative was the box
// running out of memory and taking four services down with it. Set well above
// real demand, which is one provision per project, once.
const GLOBAL_RATE = Number(process.env.GLOBAL_RATE || 20);

// Whether the id has to name a real, live artifact before it can be
// provisioned. Trust-on-first-use alone would let whoever asks first claim
// any unused id; requiring the artifact to exist means an attacker has to
// publish one first, which is rate-limited, attributable and expiring.
// Set VERIFY_ARTIFACT=0 only for local testing with no artifact service.
const VERIFY_ARTIFACT = process.env.VERIFY_ARTIFACT !== "0";

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

// Per-address provision counter. In memory on purpose: a restart clearing
// it is fine (the cap and the key check still hold), and a file would mean
// a disk write on every request, which is itself a thing to abuse.
const attempts = new Map();
let globalAttempts = [];

// What counts as "one source" for the per-source limit.
//
// A whole IPv6 /64 is one source, not one address. Every IPv6 host is
// routinely handed a /64 -- 18 quintillion addresses -- so limiting per
// address would let one machine present a fresh identity for every request
// and never see the limit at all. IPv4 (including ::ffff: mapped) is used
// whole, since there the address really is the scarce thing.
function sourceKey(address) {
  if (!address) return "unknown";
  if (address.startsWith("::ffff:")) return address.slice(7);
  if (!address.includes(":")) return address;
  return address.split(":").slice(0, 4).join(":") + "::/64";
}

// Two limits, checked together. Neither is sufficient alone: the per-source
// one is trivially escaped by anyone with more addresses, and the global one
// would let a single noisy client spend everyone's budget. Together they cost
// an attacker either scarce addresses or the whole host's budget, and both are
// bounded.
function rateLimited(address) {
  const now = Date.now();

  globalAttempts = globalAttempts.filter((t) => now - t < RATE_WINDOW_MS);
  if (globalAttempts.length >= GLOBAL_RATE) return "global";

  const key = sourceKey(address);
  const seen = (attempts.get(key) || []).filter((t) => now - t < RATE_WINDOW_MS);
  if (seen.length >= RATE_LIMIT) return "source";

  seen.push(now);
  attempts.set(key, seen);
  globalAttempts.push(now);

  // Bounded cleanup so a stream of distinct sources cannot grow this map
  // without limit -- which would be its own denial of service.
  if (attempts.size > 10000) {
    for (const [k, times] of attempts) {
      if (times.every((t) => now - t >= RATE_WINDOW_MS)) attempts.delete(k);
    }
  }
  return null;
}

// True when `id` names an artifact that is actually published and serving.
// One HEAD against the public URL -- the same thing a visitor would open --
// rather than a private API, because the artifact service is not something
// this box has credentials for.
//
// Fails closed: a network error here means "cannot confirm", and provisioning
// on an unconfirmed id is exactly what this exists to prevent.
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
  if (!(await databaseExists(name))) {
    await run("sudo", ["-u", "postgres", "psql", "-c", `CREATE DATABASE ${name}`]);
  }
  // Not gated behind the exists-check above: GoTrue's own migrations only
  // create its *tables*, never the `auth` schema they live in -- Supabase's
  // reference setup gets away with skipping this because their Postgres
  // image pre-creates it, but plain postgres:15 does not. Confirmed live:
  // without this, every migration fails with `schema "auth" does not
  // exist`. Idempotent (IF NOT EXISTS), so safe to run on a database this
  // was already done for, including one from before this line existed.
  await run("sudo", ["-u", "postgres", "psql", "-d", name, "-c", "CREATE SCHEMA IF NOT EXISTS auth"]);
}

async function containerIsRunning(name) {
  // `docker ps` without -a already excludes exited/created containers, but
  // -- confirmed live, the reason the self-healing recreate never actually
  // fired the first time -- it does NOT exclude a container stuck
  // crash-looping in "Restarting (1) ... ago": that status still shows up
  // in the default (non -a) list. `status=running` is the one filter value
  // that means what this function's name says.
  const { stdout } = await run("docker", ["ps", "-q", "-f", `name=^${name}$`, "-f", "status=running"]);
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
    // ?search_path=auth is load-bearing, not cosmetic: migrations qualify
    // every table explicitly (auth.users, auth.identities, ...), visible
    // in their own DDL, but GoTrue's runtime queries use unqualified names
    // and rely entirely on the connection's search_path to find them --
    // confirmed live, without this every query past migration time fails
    // with `relation "identities" does not exist`. Supabase's own
    // reference setup never needs this because their Postgres image sets
    // search_path at the role level (`ALTER ROLE ... SET search_path`);
    // plain postgres:15 has no such role, so it goes on the DSN instead.
    "-e", `GOTRUE_DB_DATABASE_URL=postgres://postgres@127.0.0.1:5432/${dbName}?search_path=auth`,
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
  // Reloading nginx re-parses every conf in this directory, so it is the most
  // expensive thing on the provision path and the only part of it that gets
  // worse as projects accumulate. A re-provision of a healthy project changes
  // nothing here, so doing it anyway meant the self-heal path -- the one a
  // caller can trigger over and over -- carried that cost for no reason.
  // Comparing first makes a repeat provision genuinely free.
  try {
    if ((await readFile(confPath, "utf8")) === conf) return;
  } catch {
    // Not there yet: write it.
  }
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
async function provision(id, registry) {
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
    lastSeen: new Date().toISOString(),
  };
  await saveRegistry(registry);

  return { auth_url: `${AUTH_BASE}/${id}` };
}

// ---- the reaper -------------------------------------------------------------
//
// Provisioned state had nothing to clean it up. An artifact expires after 48
// hours (an S3 lifecycle rule deletes it), but its container, its Postgres
// database and its nginx route stayed on this box forever -- so the number of
// containers only ever went up, and the machine's memory was a countdown with
// no way to add time back. That is a slow leak in ordinary use and an
// exhaustion primitive under abuse.
//
// A sweep, not a scheduled job per project: it is idempotent, it self-heals
// (a project whose deletion half-failed is simply reaped again next hour), and
// it costs one timer regardless of how many projects exist.

const REAP_INTERVAL_MS = Number(process.env.REAP_INTERVAL_MS || 60 * 60 * 1000);
// How long after its artifact stops serving a project's own resources are
// kept. Deliberately longer than the artifact's 48h: republishing to the same
// id is ordinary, and tearing down auth the moment a link lapsed would drop
// every session of a project the developer was still working on.
const REAP_AFTER_HOURS = Number(process.env.REAP_AFTER_HOURS || 72);

async function destroy(id, entry) {
  await removeContainerIfPresent(entry.containerName);
  try {
    await run("sudo", ["-u", "postgres", "psql", "-c", `DROP DATABASE IF EXISTS ${entry.dbName}`]);
  } catch (e) {
    // Reported, not fatal: a database that would not drop (an open
    // connection, most likely) should not stop the container and the route
    // from going, and the next sweep will try again.
    console.error(`reap(${id}): could not drop ${entry.dbName}:`, e.message);
  }
  try {
    await rm(path.join(NGINX_CONF_DIR, `${id}.conf`));
  } catch {
    // Already gone is the desired end state, not a failure.
  }
}

async function reap() {
  let registry;
  try {
    registry = await loadRegistry();
  } catch {
    return;
  }
  const cutoff = Date.now() - REAP_AFTER_HOURS * 60 * 60 * 1000;
  const doomed = [];
  for (const [id, entry] of Object.entries(registry)) {
    const seen = Date.parse(entry.lastSeen || entry.createdAt || 0);
    // An unparseable or missing timestamp is left alone rather than treated
    // as ancient: reaping on a date we could not read would delete live
    // projects provisioned before this field existed.
    if (!Number.isFinite(seen) || seen >= cutoff) continue;
    if (await artifactExists(id)) continue; // republished; the clock restarts
    doomed.push([id, entry]);
  }
  if (doomed.length === 0) return;

  for (const [id, entry] of doomed) {
    try {
      await destroy(id, entry);
      delete registry[id];
      console.log(`reaped ${id}`);
    } catch (e) {
      console.error(`reap(${id}) failed:`, e.message);
    }
  }
  await saveRegistry(registry);
  try {
    await run("nginx", ["-t"]);
    await run("nginx", ["-s", "reload"]);
  } catch (e) {
    console.error("reap: nginx reload failed:", e.message);
  }
}

// One reload for the whole sweep rather than one per project, and never on
// the request path -- see the comment on the sweep above.
setInterval(() => {
  reap().catch((e) => console.error("reap sweep failed:", e.message));
}, REAP_INTERVAL_MS).unref();

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
  // Every check below runs before anything is created. Provisioning starts a
  // container and a database, so the order is deliberate: the cheap refusals
  // come first, and nothing expensive happens until all of them pass.
  //
  // There is deliberately no credential here. The only thing a caller can
  // influence is the id -- port, database name, container name and JWT secret
  // are all reused from the existing registry entry -- so re-provisioning
  // someone else's project restarts their container with identical config and
  // changes nothing. A key would have been guarding a takeover that the
  // endpoint's own shape already makes impossible. What bounds the damage is
  // the cap and the rate limits, none of which need to know who is asking.
  //
  // The address comes off the socket, not X-Forwarded-For: nginx on this box
  // is the only thing in front, and a header the caller sets is not something
  // to rate-limit on.
  const limited = rateLimited(req.socket.remoteAddress);
  if (limited === "global") {
    return fail(
      res,
      429,
      `this host is provisioning at its limit of ${GLOBAL_RATE} per hour; try again shortly`
    );
  }
  if (limited === "source") {
    return fail(res, 429, `at most ${RATE_LIMIT} provisions per hour from one source`);
  }

  try {
    const registry = await loadRegistry();

    // Only counts ids we have not seen: re-provisioning an existing project
    // is a no-op that must keep working even at the cap, since that is how a
    // crash-looped container gets healed.
    if (!registry[id] && Object.keys(registry).length >= MAX_PROJECTS) {
      return fail(
        res,
        503,
        `this host is at its limit of ${MAX_PROJECTS} projects; existing ones still work`
      );
    }

    if (!registry[id] && !(await artifactExists(id))) {
      return fail(
        res,
        404,
        `no artifact is published at ${SITE_BASE}/artifacts/${id} -- publish the project first`
      );
    }

    const result = await provision(id, registry);
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
